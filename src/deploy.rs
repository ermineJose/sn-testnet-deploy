// Copyright (c) 2023, MaidSafe.
// All rights reserved.
//
// This SAFE Network Software is licensed under the BSD-3-Clause license.
// Please see the LICENSE file for more details.

use crate::{
    error::{Error, Result},
    print_duration, SnCodebaseType, TestnetDeploy,
};
use colored::Colorize;
use std::{net::SocketAddr, path::PathBuf, time::Instant};

pub struct DeployCmd {
    testnet_deploy: TestnetDeploy,
    name: String,
    node_count: u16,
    vm_count: u16,
    public_rpc: bool,
    logstash_details: Option<(String, Vec<SocketAddr>)>,
    sn_codebase_type: SnCodebaseType,
    env_variables: Option<Vec<(String, String)>>,
}

impl DeployCmd {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        testnet_deploy: TestnetDeploy,
        name: String,
        node_count: u16,
        vm_count: u16,
        public_rpc: bool,
        logstash_details: Option<(String, Vec<SocketAddr>)>,
        sn_codebase_type: SnCodebaseType,
        env_variables: Option<Vec<(String, String)>>,
    ) -> Self {
        Self {
            testnet_deploy,
            name,
            node_count,
            vm_count,
            public_rpc,
            logstash_details,
            sn_codebase_type,
            env_variables,
        }
    }

    pub async fn execute(self) -> Result<()> {
        let build_custom_binaries = {
            match &self.sn_codebase_type {
                SnCodebaseType::Main { safenode_features } => safenode_features.is_some(),
                SnCodebaseType::Branch { .. } => true,
                SnCodebaseType::Versioned { .. } => false,
            }
        };
        self.create_infra(build_custom_binaries)
            .await
            .map_err(|err| {
                println!("Failed to create infra {err:?}");
                err
            })?;

        let mut n = 1;
        let total = if build_custom_binaries { 5 } else { 4 };
        if build_custom_binaries {
            self.print_ansible_run_banner(n, total, "Build Custom Binaries");
            self.build_safe_network_binaries().await.map_err(|err| {
                println!("Failed to build safe network binaries {err:?}");
                err
            })?;
            n += 1;
        }

        self.print_ansible_run_banner(n, total, "Provision Genesis Node");
        self.provision_genesis_node().await.map_err(|err| {
            println!("Failed to provision genesis node {err:?}");
            err
        })?;
        n += 1;

        let (genesis_multiaddr, _) = self
            .testnet_deploy
            .get_genesis_multiaddr(&self.name)
            .await
            .map_err(|err| {
                println!("Failed to get genesis multiaddr {err:?}");
                err
            })?;
        println!("Obtained multiaddr for genesis node: {genesis_multiaddr}");

        let mut node_provision_failed = false;
        self.print_ansible_run_banner(n, total, "Provision Remaining Nodes");
        let result = self.provision_remaining_nodes(&genesis_multiaddr).await;
        match result {
            Ok(()) => {
                println!("Provisioned all remaining nodes");
            }
            Err(_) => {
                node_provision_failed = true;
            }
        }
        n += 1;

        self.print_ansible_run_banner(n, total, "Deploy Faucet");
        self.provision_faucet(&genesis_multiaddr)
            .await
            .map_err(|err| {
                println!("Failed to provision faucet {err:?}");
                err
            })?;
        n += 1;

        self.print_ansible_run_banner(n, total, "Provision RPC Client on Genesis Node");
        self.provision_safenode_rpc_client(&genesis_multiaddr)
            .await
            .map_err(|err| {
                println!("Failed to provision safenode rpc client {err:?}");
                err
            })?;

        self.testnet_deploy
            .list_inventory(
                &self.name,
                true,
                self.sn_codebase_type,
                Some(self.node_count),
            )
            .await
            .map_err(|err| {
                println!("Failed to list inventory {err:?}");
                err
            })?;

        if node_provision_failed {
            println!();
            println!("{}", "WARNING!".yellow());
            println!("Some nodes failed to provision without error.");
            println!("This usually means a small number of nodes failed to start on a few VMs.");
            println!("However, most of the time the deployment will still be usable.");
            println!("See the output from Ansible to determine which VMs had failures.");
        }

        Ok(())
    }

    async fn create_infra(&self, enable_build_vm: bool) -> Result<()> {
        let start = Instant::now();
        println!("Selecting {} workspace...", self.name);
        self.testnet_deploy
            .terraform_runner
            .workspace_select(&self.name)?;
        let args = vec![
            ("node_count".to_string(), self.vm_count.to_string()),
            ("use_custom_bin".to_string(), enable_build_vm.to_string()),
        ];
        println!("Running terraform apply...");
        self.testnet_deploy.terraform_runner.apply(args)?;
        print_duration(start.elapsed());
        Ok(())
    }

    async fn build_safe_network_binaries(&self) -> Result<()> {
        let start = Instant::now();
        println!("Obtaining IP address for build VM...");
        let build_inventory = self
            .testnet_deploy
            .ansible_runner
            .inventory_list(
                PathBuf::from("inventory")
                    .join(format!(".{}_build_inventory_digital_ocean.yml", self.name)),
                true,
            )
            .await?;
        let build_ip = build_inventory[0].1;
        self.testnet_deploy.ssh_client.wait_for_ssh_availability(
            &build_ip,
            &self.testnet_deploy.cloud_provider.get_ssh_user(),
        )?;

        println!("Running ansible against build VM...");
        let extra_vars = self.build_binaries_extra_vars_doc()?;
        self.testnet_deploy.ansible_runner.run_playbook(
            PathBuf::from("build.yml"),
            PathBuf::from("inventory")
                .join(format!(".{}_build_inventory_digital_ocean.yml", self.name)),
            self.testnet_deploy.cloud_provider.get_ssh_user(),
            Some(extra_vars),
        )?;
        print_duration(start.elapsed());
        Ok(())
    }

    pub async fn provision_genesis_node(&self) -> Result<()> {
        let start = Instant::now();
        let genesis_inventory = self
            .testnet_deploy
            .ansible_runner
            .inventory_list(
                PathBuf::from("inventory").join(format!(
                    ".{}_genesis_inventory_digital_ocean.yml",
                    self.name
                )),
                true,
            )
            .await?;
        let genesis_ip = genesis_inventory[0].1;
        self.testnet_deploy.ssh_client.wait_for_ssh_availability(
            &genesis_ip,
            &self.testnet_deploy.cloud_provider.get_ssh_user(),
        )?;
        self.testnet_deploy.ansible_runner.run_playbook(
            PathBuf::from("genesis_node.yml"),
            PathBuf::from("inventory").join(format!(
                ".{}_genesis_inventory_digital_ocean.yml",
                self.name
            )),
            self.testnet_deploy.cloud_provider.get_ssh_user(),
            Some(self.build_node_extra_vars_doc(None, None)?),
        )?;
        print_duration(start.elapsed());
        Ok(())
    }

    pub async fn provision_faucet(&self, genesis_multiaddr: &str) -> Result<()> {
        let start = Instant::now();
        println!("Running ansible against genesis node to deploy faucet...");
        self.testnet_deploy.ansible_runner.run_playbook(
            PathBuf::from("faucet.yml"),
            PathBuf::from("inventory").join(format!(
                ".{}_genesis_inventory_digital_ocean.yml",
                self.name
            )),
            self.testnet_deploy.cloud_provider.get_ssh_user(),
            Some(self.build_faucet_extra_vars_doc(genesis_multiaddr)?),
        )?;
        print_duration(start.elapsed());
        Ok(())
    }

    pub async fn provision_safenode_rpc_client(&self, genesis_multiaddr: &str) -> Result<()> {
        let start = Instant::now();
        println!("Running ansible against genesis node to start safenode_rpc_client service...");
        self.testnet_deploy.ansible_runner.run_playbook(
            PathBuf::from("safenode_rpc_client.yml"),
            PathBuf::from("inventory").join(format!(
                ".{}_genesis_inventory_digital_ocean.yml",
                self.name
            )),
            self.testnet_deploy.cloud_provider.get_ssh_user(),
            Some(self.build_safenode_rpc_client_extra_vars_doc(genesis_multiaddr)?),
        )?;
        print_duration(start.elapsed());
        Ok(())
    }

    pub async fn provision_remaining_nodes(&self, genesis_multiaddr: &str) -> Result<()> {
        let start = Instant::now();
        self.testnet_deploy.ansible_runner.run_playbook(
            PathBuf::from("nodes.yml"),
            PathBuf::from("inventory")
                .join(format!(".{}_node_inventory_digital_ocean.yml", self.name)),
            self.testnet_deploy.cloud_provider.get_ssh_user(),
            Some(self.build_node_extra_vars_doc(
                Some(genesis_multiaddr.to_string()),
                Some(self.node_count),
            )?),
        )?;
        print_duration(start.elapsed());
        Ok(())
    }

    /// Helpers

    fn print_ansible_run_banner(&self, n: usize, total: usize, s: &str) {
        let ansible_run_msg = format!("Ansible Run {} of {}: ", n, total);
        let line = "=".repeat(s.len() + ansible_run_msg.len());
        println!("{}\n{}{}\n{}", line, ansible_run_msg, s, line);
    }

    fn build_binaries_extra_vars_doc(&self) -> Result<String> {
        let mut extra_vars = String::new();
        extra_vars.push_str("{ ");

        match &self.sn_codebase_type {
            SnCodebaseType::Main { safenode_features } => {
                if let Some(features) = safenode_features {
                    Self::add_value(&mut extra_vars, "custom_bin", "true");
                    Self::add_value(&mut extra_vars, "testnet_name", &self.name);
                    // Manually specifying the default value from ansible cfg to make things clear.
                    Self::add_value(&mut extra_vars, "org", "maidsafe");
                    Self::add_value(&mut extra_vars, "branch", "main");
                    Self::add_value(&mut extra_vars, "safenode_features_list", features);
                } else {
                    Self::add_value(&mut extra_vars, "custom_bin", "false");
                }
            }
            SnCodebaseType::Branch {
                repo_owner,
                branch,
                safenode_features,
            } => {
                Self::add_value(&mut extra_vars, "custom_bin", "true");
                Self::add_value(&mut extra_vars, "testnet_name", &self.name);
                Self::add_value(&mut extra_vars, "org", repo_owner);
                Self::add_value(&mut extra_vars, "branch", branch);
                if let Some(features) = safenode_features {
                    Self::add_value(&mut extra_vars, "safenode_features_list", features);
                }
            }
            SnCodebaseType::Versioned { .. } => {
                Self::add_value(&mut extra_vars, "custom_bin", "false");
            }
        };

        let mut extra_vars = extra_vars.strip_suffix(", ").unwrap().to_string();
        extra_vars.push_str(" }");

        Ok(extra_vars)
    }

    fn build_node_extra_vars_doc(
        &self,
        genesis_multiaddr: Option<String>,
        node_instance_count: Option<u16>,
    ) -> Result<String> {
        let mut extra_vars = String::new();
        extra_vars.push_str("{ ");
        Self::add_value(
            &mut extra_vars,
            "provider",
            &self.testnet_deploy.cloud_provider.to_string(),
        );
        Self::add_value(&mut extra_vars, "testnet_name", &self.name);
        if genesis_multiaddr.is_some() {
            Self::add_value(
                &mut extra_vars,
                "genesis_multiaddr",
                &genesis_multiaddr.ok_or_else(|| Error::GenesisMultiAddrNotSupplied)?,
            );
        }
        if node_instance_count.is_some() {
            Self::add_value(
                &mut extra_vars,
                "node_instance_count",
                &node_instance_count.unwrap_or(20).to_string(),
            );
        }
        // The default inside ansible is false
        if self.public_rpc {
            Self::add_value(&mut extra_vars, "public_rpc", "true");
        }

        match &self.sn_codebase_type {
            SnCodebaseType::Main { safenode_features } => {
                let node_archive_url = if safenode_features.is_some() {
                    format!(
                        "https://sn-node.s3.eu-west-2.amazonaws.com/maidsafe/main/safenode-{}-x86_64-unknown-linux-musl.tar.gz",
                        self.name)
                } else {
                    // This value is predefined inside ansible cfg, but manually writing it down for clarity.
                    // Get the default
                    "https://sn-node.s3.eu-west-2.amazonaws.com/safenode-latest-x86_64-unknown-linux-musl.tar.gz".to_string()
                };
                Self::add_value(&mut extra_vars, "node_archive_url", &node_archive_url);
            }
            SnCodebaseType::Branch {
                repo_owner, branch, ..
            } => {
                let node_archive_url = format!(
                    "https://sn-node.s3.eu-west-2.amazonaws.com/{}/{}/safenode-{}-x86_64-unknown-linux-musl.tar.gz",
                    repo_owner,
                    branch,
                    self.name);
                Self::add_value(&mut extra_vars, "node_archive_url", &node_archive_url);
            }
            SnCodebaseType::Versioned {
                safenode_version, ..
            } => {
                // The manager supports `--version`, so we don't need to pass the GitHub release URL.
                Self::add_value(&mut extra_vars, "version", safenode_version);
            }
        };

        // add in node manager url
        match &self.sn_codebase_type {
            SnCodebaseType::Branch {
                repo_owner, branch, ..
            } => {
                Self::add_value(&mut extra_vars, "branch", branch);
                Self::add_value(&mut extra_vars, "org", repo_owner);
                Self::add_value(
                &mut extra_vars,
                "node_manager_archive_url",
                &format!(
                    "https://sn-node.s3.eu-west-2.amazonaws.com/{}/{}/safenode-manager-{}-x86_64-unknown-linux-musl.tar.gz",
                    repo_owner,
                    branch,
                    &self.name),
            );
            }
            _ => {
                Self::add_value(
                    &mut extra_vars,
                    "node_manager_archive_url",
                    "https://sn-node-manager.s3.eu-west-2.amazonaws.com/safenode-manager-latest-x86_64-unknown-linux-musl.tar.gz",
                );
            }
        }

        // add in node manager daemon url
        match &self.sn_codebase_type {
            SnCodebaseType::Branch {
                repo_owner, branch, ..
            } => {
                Self::add_value(&mut extra_vars, "branch", branch);
                Self::add_value(&mut extra_vars, "org", repo_owner);
                Self::add_value(
                &mut extra_vars,
                "node_manager_daemon_archive_url",
                &format!(
                    "https://sn-node.s3.eu-west-2.amazonaws.com/{}/{}/safenode-manager-daemon-{}-x86_64-unknown-linux-musl.tar.gz",
                    repo_owner,
                    branch,
                    &self.name),
            );
            }
            _ => {
                Self::add_value(
                    &mut extra_vars,
                    "node_manager_archive_url",
                    "https://sn-node-manager.s3.eu-west-2.amazonaws.com/safenode-manager-daemon-latest-x86_64-unknown-linux-musl.tar.gz",
                );
            }
        }

        if let Some(env_vars) = &self.env_variables {
            // the values are sanitized and reconstructed here. Better to error out at the deployer than at the manager.
            let mut env_vars_strs = Vec::new();
            for (key, val) in env_vars {
                env_vars_strs.push(format!("{key}={val}"));
            }
            Self::add_value(&mut extra_vars, "env_variables", &env_vars_strs.join(","));
        }

        if let Some((logstash_stack_name, logstash_hosts)) = &self.logstash_details {
            Self::add_value(&mut extra_vars, "logstash_stack_name", logstash_stack_name);
            extra_vars.push_str("\"logstash_hosts\": [");
            for host in logstash_hosts.iter() {
                extra_vars.push_str(&format!("\"{}\", ", host));
            }
            let mut extra_vars = extra_vars.strip_suffix(", ").unwrap().to_string();
            extra_vars.push(']');
        }
        extra_vars.push('}');

        Ok(extra_vars)
    }

    fn build_faucet_extra_vars_doc(&self, genesis_multiaddr: &str) -> Result<String> {
        let mut extra_vars = String::new();
        extra_vars.push_str("{ ");
        Self::add_value(
            &mut extra_vars,
            "provider",
            &self.testnet_deploy.cloud_provider.to_string(),
        );
        Self::add_value(&mut extra_vars, "testnet_name", &self.name);
        Self::add_value(&mut extra_vars, "genesis_multiaddr", genesis_multiaddr);
        match &self.sn_codebase_type {
            SnCodebaseType::Branch {
                repo_owner, branch, ..
            } => {
                Self::add_value(&mut extra_vars, "branch", branch);
                Self::add_value(&mut extra_vars, "org", repo_owner);
                Self::add_value(
                &mut extra_vars,
                "faucet_archive_url",
                &format!(
                    "https://sn-node.s3.eu-west-2.amazonaws.com/{}/{}/faucet-{}-x86_64-unknown-linux-musl.tar.gz",
                    repo_owner,
                    branch,
                    &self.name),
            );
            }
            _ => {
                Self::add_value(
                    &mut extra_vars,
                    "faucet_archive_url",
                    "https://sn-faucet.s3.eu-west-2.amazonaws.com/faucet-latest-x86_64-unknown-linux-musl.tar.gz",
                );
            }
        }

        let mut extra_vars = extra_vars.strip_suffix(", ").unwrap().to_string();
        extra_vars.push_str(" }");
        Ok(extra_vars)
    }

    fn build_safenode_rpc_client_extra_vars_doc(&self, genesis_multiaddr: &str) -> Result<String> {
        let mut extra_vars = String::new();
        extra_vars.push_str("{ ");
        Self::add_value(
            &mut extra_vars,
            "provider",
            &self.testnet_deploy.cloud_provider.to_string(),
        );
        Self::add_value(&mut extra_vars, "testnet_name", &self.name);
        Self::add_value(&mut extra_vars, "genesis_multiaddr", genesis_multiaddr);
        match &self.sn_codebase_type {
            SnCodebaseType::Branch {
                repo_owner, branch, ..
            } => {
                Self::add_value(&mut extra_vars, "branch", branch);
                Self::add_value(&mut extra_vars, "org", repo_owner);
                Self::add_value(
                &mut extra_vars,
                "safenode_rpc_client_archive_url",
                &format!(
                    "https://sn-node.s3.eu-west-2.amazonaws.com/{}/{}/safenode_rpc_client-{}-x86_64-unknown-linux-musl.tar.gz",
                    repo_owner,
                    branch,
                    &self.name),
            );
            }
            _ => {
                Self::add_value(
                    &mut extra_vars,
                    "safenode_rpc_client_archive_url",
                    "https://sn-node-rpc-client.s3.eu-west-2.amazonaws.com/safenode_rpc_client-latest-x86_64-unknown-linux-musl.tar.gz",);
            }
        }

        let mut extra_vars = extra_vars.strip_suffix(", ").unwrap().to_string();
        extra_vars.push_str(" }");
        Ok(extra_vars)
    }

    fn add_value(document: &mut String, name: &str, value: &str) {
        document.push_str(&format!("\"{name}\": \"{value}\", "))
    }
}
