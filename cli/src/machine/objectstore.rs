// Copyright 2024 ADM Contributors
// SPDX-License-Identifier: Apache-2.0, MIT

use anyhow::anyhow;
use async_tempfile::TempFile;
use cid::multihash::{Code, MultihashDigest};
use cid::Cid;
use clap::{Args, Parser, Subcommand};
use clap_stdin::FileOrStdin;
use console::{style, Emoji};
use fendermint_actor_machine::WriteAccess;
use fendermint_actor_objectstore::{GetParams, ListParams, Object, ObjectKind, PutParams};
use fendermint_vm_message::query::FvmQueryHeight;
use fvm_ipld_encoding::serde_bytes::ByteBuf;
use fvm_shared::address::Address;
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::json;
use std::sync::Arc;
use tendermint_rpc::HttpClient;
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use tokio::task::LocalSet;

use adm_provider::{json_rpc::JsonRpcProvider, upload::ObjectClient, BroadcastMode};
use adm_sdk::machine::{objectstore::generate_cid, objectstore::ObjectStore, Machine};

use crate::{get_signer, parse_address, print_json, Cli};

const MAX_INTERNAL_OBJECT_LENGTH: u64 = 1024;

#[derive(Clone, Debug, Args)]
pub struct ObjectstoreArgs {
    #[command(subcommand)]
    command: ObjectstoreCommands,
}

#[derive(Clone, Debug, Subcommand)]
enum ObjectstoreCommands {
    Create(ObjectstoreCreateArgs),
    Put(ObjectstorePutArgs),
    Get(ObjectstoreGetArgs),
    List(ObjectstoreListArgs),
}

#[derive(Clone, Debug, Args)]
struct ObjectstoreCreateArgs {
    #[arg(long, default_value_t = false)]
    public_write: bool,
}

#[derive(Clone, Debug, Parser)]
struct ObjectstorePutArgs {
    #[arg(short, long, value_parser = parse_address)]
    address: Address,
    #[arg(short, long)]
    key: String,
    #[arg(short, long, action)]
    overwrite: bool,
    #[clap(default_value = "-")]
    input: FileOrStdin,
}

#[derive(Clone, Debug, Args)]
struct ObjectstoreGetArgs {
    #[arg(short, long, value_parser = parse_address)]
    address: Address,
    #[arg(short, long)]
    key: String,
}

#[derive(Clone, Debug, Args)]
struct ObjectstoreListArgs {
    #[arg(short, long, value_parser = parse_address)]
    address: Address,
    #[arg(short, long, default_value = "")]
    prefix: String,
    #[arg(short, long, default_value = "/")]
    delimiter: String,
    #[arg(short, long, default_value_t = 0)]
    offset: u64,
    #[arg(short, long, default_value_t = 0)]
    limit: u64,
}

pub async fn handle_objectstore(cli: Cli, args: &ObjectstoreArgs) -> anyhow::Result<()> {
    let provider = JsonRpcProvider::new_http(cli.rpc_url.clone(), None)?;

    match &args.command {
        ObjectstoreCommands::Create(args) => {
            let mut signer = get_signer(&provider, cli.wallet_pk, cli.chain_name).await?;
            let write_access = if args.public_write {
                WriteAccess::Public
            } else {
                WriteAccess::OnlyOwner
            };
            let (store, tx) =
                ObjectStore::new(&provider, &mut signer, write_access, Default::default()).await?;

            print_json(&json!({"address": store.address().to_string(), "tx": &tx}))
        }
        ObjectstoreCommands::Put(ObjectstorePutArgs {
            key,
            address,
            overwrite,
            input,
        }) => {
            let mut signer =
                get_signer(&provider, cli.wallet_pk.clone(), cli.chain_name.clone()).await?;
            let object_client = ObjectClient::new(cli.object_api_url, 1942764459484029);
            let machine = ObjectStore::<HttpClient>::attach(*address);
            let mut reader = input.into_async_reader().await?;
            let mut first_chunk = vec![0; MAX_INTERNAL_OBJECT_LENGTH as usize];
            let upload_progress = UploadProgressBar::new();

            match reader.read_exact(&mut first_chunk).await {
                Ok(first_chunk_size) => {
                    let (tx, rx) = mpsc::channel::<Vec<u8>>(1024);
                    let bytes_read = Arc::new(Mutex::new(0 as usize));
                    let cid = Arc::new(Mutex::new(Cid::new_v0(Code::Sha2_256.digest(&[]))?));

                    // Spawn the non-Send future within the local task set
                    // This is necessary because the clap's AsyncReader impl
                    // is not `Send`
                    let bytes_read_clone = bytes_read.clone();
                    let cid_clone = cid.clone();
                    let local_set = LocalSet::new();
                    let chunk = first_chunk[..first_chunk_size].to_vec();
                    local_set.spawn_local(async {
                        match process_read_chunk(reader, tx, chunk, bytes_read_clone, cid_clone)
                            .await
                        {
                            Ok(_) => {}
                            Err(e) => {
                                panic!("Error reading from input: {:?}", e);
                            }
                        }
                    });
                    local_set.await;

                    let object_cid = cid.lock().await.clone();
                    let params = PutParams {
                        key: key.as_bytes().to_vec(),
                        kind: ObjectKind::External(object_cid),
                        overwrite: *overwrite,
                    };
                    let total_bytes = first_chunk_size as usize + *bytes_read.lock().await;
                    let response_cid = machine
                        .object_upload(
                            &mut signer,
                            object_client,
                            key.clone(),
                            object_cid,
                            rx,
                            total_bytes,
                            params.clone(),
                        )
                        .await?;

                    upload_progress.finish(response_cid, object_cid);

                    let tx = machine
                        .put(
                            &provider,
                            &mut signer,
                            params,
                            BroadcastMode::Commit,
                            Default::default(),
                        )
                        .await?;

                    print_json(&tx).unwrap();
                }
                Err(e) => {
                    // internal object
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        reader.read_to_end(&mut first_chunk).await?;

                        let tx = machine
                            .put(
                                &provider,
                                &mut signer,
                                PutParams {
                                    key: key.as_bytes().to_vec(),
                                    kind: ObjectKind::Internal(ByteBuf(first_chunk)),
                                    overwrite: *overwrite,
                                },
                                BroadcastMode::Commit,
                                Default::default(),
                            )
                            .await?;

                        print_json(&tx).unwrap();
                    } else {
                        return Err(e.into());
                    }
                }
            }
            Ok(())
        }
        ObjectstoreCommands::Get(args) => {
            // TODO: Handle range requests
            // TODO: Show progress bar?
            let machine = ObjectStore::<HttpClient>::attach(args.address);
            let key = args.key.as_str();
            let object = machine
                .get(
                    &provider,
                    GetParams {
                        key: key.as_bytes().to_vec(),
                    },
                    FvmQueryHeight::Committed,
                )
                .await?;

            if let Some(object) = object {
                match object {
                    Object::Internal(buf) => {
                        let mut stdout = io::stdout();
                        stdout.write_all(&buf.0).await?;
                        Ok(())
                    }
                    Object::External((buf, resolved)) => {
                        let cid = Cid::try_from(buf.0)?;
                        let cid = cid.to_string();
                        if !resolved {
                            return Err(anyhow!("object is not resolved"));
                        }

                        print_json(&json!({"cid": cid}))
                        // TODO: Get cid from ObjectApi
                    }
                }
            } else {
                Err(anyhow!("object not found for key '{}'", key))
            }
        }
        ObjectstoreCommands::List(args) => {
            let machine = ObjectStore::<HttpClient>::attach(args.address);
            let prefix = args.prefix.as_str();
            let delimiter = args.delimiter.as_str();
            let list = machine
                .list(
                    &provider,
                    ListParams {
                        prefix: prefix.as_bytes().to_vec(),
                        delimiter: delimiter.as_bytes().to_vec(),
                        offset: args.offset,
                        limit: args.limit,
                    },
                    FvmQueryHeight::Committed,
                )
                .await?;
            // TODO: ObjectList doesn't need to return as an Option. We can change this in the actor.

            if let Some(list) = list {
                print_json(&list)
            } else {
                Err(anyhow!("object list not found for prefix '{}'", prefix))
            }
        }
    }
}

async fn process_read_chunk(
    mut reader: impl AsyncRead + Unpin,
    tx: Sender<Vec<u8>>,
    first_chunk: Vec<u8>,
    bytes_read: Arc<Mutex<usize>>,
    cid: Arc<Mutex<Cid>>,
) -> anyhow::Result<()> {
    // create a tmpfile to help with CID calculation
    let mut tmp = TempFile::new().await?;
    let mut upload_buffer = vec![0; 10 * 1024 * 1024];

    // write first chunk to temp file and mpsc channel
    tx.send(first_chunk.clone()).await.unwrap();
    tmp.write_all(&first_chunk).await?;

    // read remaining bytes from the reader into temp file and mpsc channel
    loop {
        match reader.read(&mut upload_buffer).await {
            Ok(0) => {
                break;
            }
            Ok(n) => {
                if tx.send(upload_buffer[..n].to_vec()).await.is_err() {
                    return Err(anyhow!("error sending data to channel"))?;
                }
                let mut bytes = bytes_read.lock().await;
                *bytes += n;
                tmp.write_all(&upload_buffer[..n]).await?;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    tmp.flush().await?;
    tmp.rewind().await?;
    let generated_cid = generate_cid(&mut tmp).await?;
    cid.lock().await.clone_from(&generated_cid);

    Ok(())
}

static CHECKMARK: Emoji<'_, '_> = Emoji("✅  ", "");
static TRUCK: Emoji<'_, '_> = Emoji("🚚  ", "");
static FINISHED: Emoji<'_, '_> = Emoji("🎉  ", "");

struct UploadProgressBar {
    inner: ProgressBar,
}

impl UploadProgressBar {
    fn new() -> Self {
        let inner = ProgressBar::new_spinner();
        inner.set_style(
            ProgressStyle::with_template("{spinner:.green} [{elapsed_precise}]")
                .unwrap()
                .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
        );
        inner.println(format!(
            "{} {}Uploading object...",
            style("[1/2]").bold().dim(),
            TRUCK,
        ));
        inner.enable_steady_tick(std::time::Duration::from_millis(80));

        Self { inner }
    }

    fn finish(&self, response_cid: Cid, request_cid: Cid) {
        self.inner.println(format!(
            "{} {}Uploaded object with CID: {}",
            style("[1/2]").bold().dim(),
            FINISHED,
            response_cid
        ));
        assert!(response_cid == request_cid);
        self.inner.println(format!(
            "{} {}Local CID matched with remote CID",
            style("[1/2]").bold().dim(),
            CHECKMARK,
        ));
        self.inner.finish_and_clear();
    }
}
