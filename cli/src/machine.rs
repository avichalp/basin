// Copyright 2024 ADM Contributors
// SPDX-License-Identifier: Apache-2.0, MIT

use clap::{Args, Subcommand};
use fendermint_vm_message::query::FvmQueryHeight;
use fvm_shared::address::Address;
use serde_json::json;

use adm_provider::json_rpc::JsonRpcProvider;
use adm_sdk::Adm;

use crate::machine::{
    accumulator::{handle_accumulator, AccumulatorArgs},
    objectstore::{handle_objectstore, ObjectstoreArgs},
};
use crate::{parse_address, print_json, Cli};

pub mod accumulator;
pub mod objectstore;

#[derive(Clone, Debug, Args)]
pub struct MachineArgs {
    #[command(subcommand)]
    command: MachineCommands,
}

#[derive(Clone, Debug, Subcommand)]
enum MachineCommands {
    /// Get machine info.
    Get(GetMachineArgs),
    /// List machines by owner.
    List(ListMachineArgs),
    /// Object store related commands.
    Objectstore(ObjectstoreArgs),
    /// Accumulator related commands.
    Accumulator(AccumulatorArgs),
}

#[derive(Clone, Debug, Args)]
struct GetMachineArgs {
    /// Machine address.
    #[arg(short, long, value_parser = parse_address)]
    address: Address,
}

#[derive(Clone, Debug, Args)]
struct ListMachineArgs {
    /// Owner address.
    #[arg(short, long, value_parser = parse_address)]
    owner: Address,
}

pub async fn handle_machine(cli: Cli, args: &MachineArgs) -> anyhow::Result<()> {
    match &args.command {
        MachineCommands::Get(args) => {
            let provider = JsonRpcProvider::new_http(cli.rpc_url, None)?;
            let metadata =
                Adm::get_machine_metadata(&provider, args.address, FvmQueryHeight::Committed)
                    .await?;

            print_json(
                &json!({"kind": metadata.kind.to_string(), "owner": metadata.owner.to_string()}),
            )
        }
        MachineCommands::List(args) => {
            let provider = JsonRpcProvider::new_http(cli.rpc_url, None)?;
            let metadata =
                Adm::list_machine_metadata(&provider, args.owner, FvmQueryHeight::Committed)
                    .await?;

            print_json(&metadata)
        }
        MachineCommands::Objectstore(args) => handle_objectstore(cli, args).await,
        MachineCommands::Accumulator(args) => handle_accumulator(cli, args).await,
    }
}
