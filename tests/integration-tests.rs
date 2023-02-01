use std::{
    fs::File,
    io::BufReader,
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};

use clap::Parser;
use cli_batteries::{reset_shutdown, shutdown};
use ethers::{
    abi::Address,
    core::abi::Abi,
    prelude::{
        Bytes, ContractFactory, Http, LocalWallet, NonceManagerMiddleware, Provider, Signer,
        SignerMiddleware,
    },
    providers::Middleware,
    types::{BlockNumber, Filter, H160, H256, Log, U256},
    utils::{Anvil, AnvilInstance},
};
use eyre::{bail, Result as AnyhowResult};
use hyper::{Body, Client, client::HttpConnector, Request};
use semaphore::{merkle_tree::Branch, poseidon_tree::PoseidonTree};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tempfile::tempdir;
use tokio::{spawn, task::JoinHandle};
use tracing::{error, info, instrument};
use tracing_subscriber::fmt::{format::FmtSpan, time::Uptime};
use url::{Host, Url};

use signup_sequencer::{app::App, identity_tree::Hash, Options, server};

const TEST_LEAFS: &[&str] = &[
    "0000F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0F0",
    "0000F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1F1",
    "0000F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2F2",
];

#[tokio::test]
#[serial_test::serial]
async fn insert_identity_and_proofs() {
    // Initialize logging for the test.
    init_tracing_subscriber();
    info!("Starting integration test");

    let db_dir = tempdir().unwrap();
    let db = db_dir.path().join("test.db");

    let mut options = Options::try_parse_from([
        "signup-sequencer",
        "--database",
        &format!("sqlite://{}", db.to_str().unwrap()),
        "--database-max-connections",
        "1",
    ])
    .expect("Failed to create options");
    options.server.server = Url::parse("http://127.0.0.1:0/").expect("Failed to parse URL");

    let (chain, private_key, semaphore_address) = spawn_mock_chain()
        .await
        .expect("Failed to spawn ganache chain");

    options.app.contracts.semaphore_address = semaphore_address;
    options.app.ethereum.read_options.confirmation_blocks_delay = 2;
    options.app.ethereum.read_options.ethereum_provider =
        Url::parse(&chain.endpoint()).expect("Failed to parse ganache endpoint");
    options.app.ethereum.write_options.signing_key = private_key;

    let (app, local_addr) = spawn_app(options.clone())
        .await
        .expect("Failed to spawn app.");

    let uri = "http://".to_owned() + &local_addr.to_string();
    let mut ref_tree = PoseidonTree::new(22, options.app.contracts.initial_leaf_value);
    let client = Client::new();
    test_inclusion_proof(
        &uri,
        &client,
        0,
        &mut ref_tree,
        &options.app.contracts.initial_leaf_value,
        true,
    )
    .await;
    test_inclusion_proof(
        &uri,
        &client,
        1,
        &mut ref_tree,
        &options.app.contracts.initial_leaf_value,
        true,
    )
    .await;
    test_insert_identity(&uri, &client, &mut ref_tree, 0).await;
    test_inclusion_proof(
        &uri,
        &client,
        0,
        &mut ref_tree,
        &Hash::from_str_radix(TEST_LEAFS[0], 16).expect("Failed to parse Hash from test leaf 0"),
        false,
    )
    .await;
    test_insert_identity(&uri, &client, &mut ref_tree, 1).await;
    test_inclusion_proof(
        &uri,
        &client,
        1,
        &mut ref_tree,
        &Hash::from_str_radix(TEST_LEAFS[1], 16).expect("Failed to parse Hash from test leaf 1"),
        false,
    )
    .await;
    test_inclusion_proof(
        &uri,
        &client,
        2,
        &mut ref_tree,
        &options.app.contracts.initial_leaf_value,
        true,
    )
    .await;

    // Shutdown app and reset mock shutdown
    info!("Stopping app");
    shutdown();
    app.await.unwrap();
    reset_shutdown();

    // Test loading state from file, onchain tree has leafs
    info!("Starting app");
    let (app, local_addr) = spawn_app(options.clone())
        .await
        .expect("Failed to spawn app.");
    let uri = "http://".to_owned() + &local_addr.to_string();
    info!(?uri, "App started");

    test_inclusion_proof(
        &uri,
        &client,
        0,
        &mut ref_tree,
        &Hash::from_str_radix(TEST_LEAFS[0], 16).expect("Failed to parse Hash from test leaf 0"),
        false,
    )
    .await;
    test_inclusion_proof(
        &uri,
        &client,
        1,
        &mut ref_tree,
        &Hash::from_str_radix(TEST_LEAFS[1], 16).expect("Failed to parse Hash from test leaf 1"),
        false,
    )
    .await;

    // Shutdown app and reset mock shutdown
    shutdown();
    app.await.unwrap();
    reset_shutdown();

    // Test loading state from tree, onchain tree has leafs

    info!("Starting app");
    let (app, local_addr) = spawn_app(options.clone())
        .await
        .expect("Failed to spawn app.");
    let uri = "http://".to_owned() + &local_addr.to_string();
    info!(?uri, "App started");

    test_inclusion_proof(
        &uri,
        &client,
        0,
        &mut ref_tree,
        &Hash::from_str_radix(TEST_LEAFS[0], 16).expect("Failed to parse Hash from test leaf 0"),
        false,
    )
    .await;
    test_inclusion_proof(
        &uri,
        &client,
        1,
        &mut ref_tree,
        &Hash::from_str_radix(TEST_LEAFS[1], 16).expect("Failed to parse Hash from test leaf 1"),
        false,
    )
    .await;

    // Shutdown app and reset mock shutdown
    shutdown();
    app.await.unwrap();
    reset_shutdown();
}

#[instrument(skip_all)]
async fn wait_for_log_count(
    provider: &Provider<Http>,
    semaphore_address: H160,
    expected_count: usize,
) {
    for i in 1..21 {
        let filter = Filter::new()
            .address(semaphore_address)
            .from_block(BlockNumber::Earliest)
            .to_block(BlockNumber::Latest);
        let result: Vec<Log> = provider.request("eth_getLogs", [filter]).await.unwrap();

        if result.len() >= expected_count {
            info!(
                "Got {} logs (vs expected {}), done in iteration {}: {:?}",
                result.len(),
                expected_count,
                i,
                result
            );

            // TODO: Figure out a better way to do this.
            // Getting a log event is not enough. The app waits for 1 transaction
            // confirmation. It will arrive only after the first poll interval.
            // The DEFAULT_POLL_INTERVAL in ethers-providers is 7 seconds.
            tokio::time::sleep(Duration::from_secs(8)).await;

            return;
        }

        info!(
            "Got {} logs (vs expected {}), waiting 1 second, iteration {}",
            result.len(),
            expected_count,
            i
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    panic!("Failed waiting for {} log events", expected_count);
}

#[instrument(skip_all)]
async fn test_inclusion_proof(
    uri: &str,
    client: &Client<HttpConnector>,
    leaf_index: usize,
    ref_tree: &mut PoseidonTree,
    leaf: &Hash,
    expect_failure: bool,
) {
    let mut mined_json = None;
    for i in 1..21 {
        let body = construct_inclusion_proof_body(leaf);
        info!(?uri, "Contacting");
        let req = Request::builder()
            .method("POST")
            .uri(uri.to_owned() + "/inclusionProof")
            .header("Content-Type", "application/json")
            .body(body)
            .expect("Failed to create inclusion proof hyper::Body");
        let mut response = client
            .request(req)
            .await
            .expect("Failed to execute request.");
        if expect_failure {
            assert!(!response.status().is_success());
            return;
        } else {
            assert!(response.status().is_success());
        }

        let bytes = hyper::body::to_bytes(response.body_mut())
            .await
            .expect("Failed to convert response body to bytes");
        let result = String::from_utf8(bytes.into_iter().collect())
            .expect("Could not parse response bytes to utf-8");
        let result_json = serde_json::from_str::<serde_json::Value>(&result)
            .expect("Failed to parse response as json");
        let status = result_json["status"]
            .as_str()
            .expect("Failed to get status");

        if status == "pending" {
            assert_eq!(
                result_json,
                generate_reference_proof_json(ref_tree, leaf_index, "pending")
            );
            info!("Got pending, waiting 1 second, iteration {}", i);
            tokio::time::sleep(Duration::from_secs(1)).await;
        } else {
            mined_json = Some(result_json);
            break;
        }
    }

    let result_json = mined_json.expect("Failed to get mined response");
    let proof_json = generate_reference_proof_json(ref_tree, leaf_index, "mined");
    assert_eq!(result_json, proof_json);
}

#[instrument(skip_all)]
async fn test_insert_identity(
    uri: &str,
    client: &Client<HttpConnector>,
    ref_tree: &mut PoseidonTree,
    leaf_index: usize,
) {
    let body = construct_insert_identity_body(TEST_LEAFS[leaf_index]);
    let req = Request::builder()
        .method("POST")
        .uri(uri.to_owned() + "/insertIdentity")
        .header("Content-Type", "application/json")
        .body(body)
        .expect("Failed to create insert identity hyper::Body");

    let mut response = client
        .request(req)
        .await
        .expect("Failed to execute request.");
    let bytes = hyper::body::to_bytes(response.body_mut())
        .await
        .expect("Failed to convert response body to bytes");
    let result = String::from_utf8(bytes.into_iter().collect())
        .expect("Could not parse response bytes to utf-8");
    if !response.status().is_success() {
        panic!("Failed to insert identity: {}", result);
    }
    let result_json = serde_json::from_str::<serde_json::Value>(&result)
        .expect("Failed to parse response as json");

    ref_tree.set(
        leaf_index,
        Hash::from_str_radix(TEST_LEAFS[leaf_index], 16).unwrap(),
    );

    let expected_json = generate_reference_proof_json(ref_tree, leaf_index, "pending");

    assert_eq!(result_json, expected_json);
}

fn construct_inclusion_proof_body(identity_commitment: &Hash) -> Body {
    Body::from(
        json!({
            "groupId": 1,
            "identityCommitment": identity_commitment,
        })
        .to_string(),
    )
}

fn construct_insert_identity_body(identity_commitment: &str) -> Body {
    Body::from(
        json!({
            "groupId": 1,
            "identityCommitment": identity_commitment,

        })
        .to_string(),
    )
}

#[instrument(skip_all)]
async fn spawn_app(options: Options) -> AnyhowResult<(JoinHandle<()>, SocketAddr)> {
    let app = App::new(options.app).await.expect("Failed to create App");

    let ip: IpAddr = match options.server.server.host() {
        Some(Host::Ipv4(ip)) => ip.into(),
        Some(Host::Ipv6(ip)) => ip.into(),
        Some(_) => bail!("Cannot bind {}", options.server.server),
        None => Ipv4Addr::LOCALHOST.into(),
    };
    let port = options.server.server.port().unwrap_or(9998);
    let addr = SocketAddr::new(ip, port);
    let listener = TcpListener::bind(addr).expect("Failed to bind random port");
    let local_addr = listener.local_addr()?;

    let app = spawn({
        async move {
            info!("App thread starting");
            server::bind_from_listener(Arc::new(app), Duration::from_secs(30), listener)
                .await
                .expect("Failed to bind address");
            info!("App thread stopping");
        }
    });

    Ok((app, local_addr))
}

#[derive(Deserialize, Serialize, Debug)]
struct CompiledContract {
    abi:      Abi,
    bytecode: String,
}

fn deserialize_to_bytes(input: String) -> AnyhowResult<Bytes> {
    if input.len() >= 2 && &input[0..2] == "0x" {
        let bytes: Vec<u8> = hex::decode(&input[2..])?;
        Ok(bytes.into())
    } else {
        bail!("Expected 0x prefix")
    }
}

#[instrument(skip_all)]
async fn spawn_mock_chain() -> AnyhowResult<(AnvilInstance, H256, Address)> {
    let chain = Anvil::new().block_time(2u64).spawn();
    let private_key = H256::from_slice(&chain.keys()[0].to_be_bytes());

    let provider = Provider::<Http>::try_from(chain.endpoint())
        .expect("Failed to initialize chain endpoint")
        .interval(Duration::from_millis(500u64));

    let chain_id = provider.get_chainid().await?.as_u64();

    let wallet = LocalWallet::from(chain.keys()[0].clone()).with_chain_id(chain_id);

    // connect the wallet to the provider
    let client = SignerMiddleware::new(provider, wallet.clone());
    let client = NonceManagerMiddleware::new(client, wallet.address());
    let client = std::sync::Arc::new(client);

    let poseidon_t3_json =
        File::open("./sol/PoseidonT3.json").expect("Failed to read PoseidonT3.json");
    let poseidon_t3_json: CompiledContract =
        serde_json::from_reader(BufReader::new(poseidon_t3_json))
            .expect("Could not parse compiled PoseidonT3 contract");
    let poseidon_t3_bytecode = deserialize_to_bytes(poseidon_t3_json.bytecode)?;

    let poseidon_t3_factory =
        ContractFactory::new(poseidon_t3_json.abi, poseidon_t3_bytecode, client.clone());
    let poseidon_t3_contract = poseidon_t3_factory
        .deploy(())?
        .legacy()
        .confirmations(0usize)
        .send()
        .await?;

    let incremental_binary_tree_json =
        File::open("./sol/IncrementalBinaryTree.json").expect("Compiled contract doesn't exist");
    let incremental_binary_tree_json: CompiledContract =
        serde_json::from_reader(BufReader::new(incremental_binary_tree_json))
            .expect("Could not read contract");
    let incremental_binary_tree_bytecode = incremental_binary_tree_json.bytecode.replace(
        // Find the hex for the library address by analyzing the bytecode
        "__$618958d8226014a70a872b898165ec6838$__",
        &format!("{:?}", poseidon_t3_contract.address()).replace("0x", ""),
    );
    let incremental_binary_tree_bytecode = deserialize_to_bytes(incremental_binary_tree_bytecode)?;
    let incremental_binary_tree_factory = ContractFactory::new(
        incremental_binary_tree_json.abi,
        incremental_binary_tree_bytecode,
        client.clone(),
    );
    let incremental_binary_tree_contract = incremental_binary_tree_factory
        .deploy(())?
        .legacy()
        .confirmations(0usize)
        .send()
        .await?;

    let semaphore_json =
        File::open("./sol/Semaphore.json").expect("Compiled contract doesn't exist");
    let semaphore_json: CompiledContract =
        serde_json::from_reader(BufReader::new(semaphore_json)).expect("Could not read contract");

    let semaphore_bytecode = semaphore_json.bytecode.replace(
        "__$4c0484323457fe1a856f46a4759b553fe4$__",
        &format!("{:?}", incremental_binary_tree_contract.address()).replace("0x", ""),
    );
    let semaphore_bytecode = deserialize_to_bytes(semaphore_bytecode)?;

    // create a factory which will be used to deploy instances of the contract
    let semaphore_factory =
        ContractFactory::new(semaphore_json.abi, semaphore_bytecode, client.clone());

    let semaphore_contract = semaphore_factory
        .deploy(())?
        .legacy()
        .confirmations(0usize)
        .send()
        .await?;

    // Create a group with id 1
    let group_id = U256::from(1_u64);
    let depth = 21_u8;
    let initial_leaf = U256::from(0_u64);
    semaphore_contract
        .method::<_, ()>("createGroup", (group_id, depth, initial_leaf))?
        .legacy()
        .send()
        .await? // Send TX
        .await?; // Wait for TX to be mined

    Ok((chain, private_key, semaphore_contract.address()))
}

fn init_tracing_subscriber() {
    let result = tracing_subscriber::fmt()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_line_number(true)
        .with_env_filter("info,signup_sequencer=debug")
        .with_timer(Uptime::default())
        .pretty()
        .try_init();
    if let Err(error) = result {
        error!(error, "Failed to initialize tracing_subscriber");
    }
}

fn generate_reference_proof_json(
    ref_tree: &PoseidonTree,
    leaf_idx: usize,
    status: &str,
) -> serde_json::Value {
    let proof = ref_tree
        .proof(leaf_idx)
        .unwrap()
        .0
        .iter()
        .map(|branch| match branch {
            Branch::Left(hash) => json!({ "Left": hash }),
            Branch::Right(hash) => json!({ "Right": hash }),
        })
        .collect::<Vec<_>>();
    let root = ref_tree.root();
    json!({
        "status": status,
        "root": root,
        "proof": proof,
    })
}
