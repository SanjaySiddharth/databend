// Copyright 2021 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use common_base::base::tokio;
use common_meta_api::KVApi;
use common_meta_sled_store::openraft::LogIdOptionExt;
use common_meta_sled_store::openraft::State;
use common_meta_types::protobuf::raft_service_client::RaftServiceClient;
use common_meta_types::AppliedState;
use common_meta_types::Cmd;
use common_meta_types::Endpoint;
use common_meta_types::ForwardToLeader;
use common_meta_types::LogEntry;
use common_meta_types::MetaError;
use common_meta_types::MetaRaftError;
use common_meta_types::NodeId;
use common_meta_types::RetryableError;
use common_meta_types::SeqV;
use common_meta_types::UpsertKV;
use databend_meta::configs;
use databend_meta::meta_service::meta_leader::MetaLeader;
use databend_meta::meta_service::ForwardRequest;
use databend_meta::meta_service::ForwardRequestBody;
use databend_meta::meta_service::JoinRequest;
use databend_meta::meta_service::LeaveRequest;
use databend_meta::meta_service::MetaNode;
use maplit::btreeset;
use meta_node::timeout;
use pretty_assertions::assert_eq;
use tracing::info;

use crate::init_meta_ut;
use crate::tests::meta_node;
use crate::tests::service::MetaSrvTestContext;

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_boot() -> anyhow::Result<()> {
    // - Start a single node meta service cluster.
    // - Test the single node is recorded by this cluster.

    let tc = MetaSrvTestContext::new(0);
    let addr = tc.config.raft_config.raft_api_advertise_host_endpoint();

    let mn = MetaNode::boot(&tc.config).await?;

    let got = mn.get_node(&0).await?;
    assert_eq!(addr, got.unwrap().endpoint);
    mn.stop().await?;
    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_graceful_shutdown() -> anyhow::Result<()> {
    // - Start a leader then shutdown.

    let (_nid0, tc) = meta_node::start_meta_node_leader().await?;
    let mn0 = tc.meta_node();

    let mut rx0 = mn0.raft.metrics();

    let joined = mn0.stop().await?;
    assert_eq!(3, joined);

    // tx closed:
    loop {
        let r = rx0.changed().await;
        if r.is_err() {
            info!("done!!!");
            break;
        }

        info!("st: {:?}", rx0.borrow());
    }
    assert!(rx0.changed().await.is_err());
    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_leader_and_non_voter() -> anyhow::Result<()> {
    // - Start a leader and a non-voter;
    // - Write to leader, check on non-voter.

    let (_nid0, tc0) = meta_node::start_meta_node_leader().await?;
    let mn0 = tc0.meta_node();

    let (_nid1, tc1) = meta_node::start_meta_node_non_voter(mn0.clone(), 1).await?;
    let mn1 = tc1.meta_node();

    assert_upsert_kv_synced(vec![mn0.clone(), mn1.clone()], "metakey2").await?;

    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_write_to_local_leader() -> anyhow::Result<()> {
    // - Start a leader, 2 followers and a non-voter;
    // - Write to the raft node on the leader, expect Ok.
    // - Write to the raft node on the non-leader, expect ForwardToLeader error.

    let (mut _nlog, tcs) =
        meta_node::start_meta_node_cluster(btreeset![0, 1, 2], btreeset![3]).await?;
    let all = test_context_nodes(&tcs);

    let leader_id = all[0].raft.metrics().borrow().current_leader.unwrap();

    // test writing to leader and non-leader
    let key = "t-non-leader-write";
    for id in 0u64..4 {
        let mn = &all[id as usize];
        let maybe_leader = MetaLeader::new(mn);
        let rst = maybe_leader
            .write(LogEntry {
                txid: None,
                time_ms: None,
                cmd: Cmd::UpsertKV(UpsertKV::update(&key, key.as_bytes())),
            })
            .await;

        if id == leader_id {
            assert!(rst.is_ok());
        } else {
            assert!(rst.is_err());
            let e = rst.unwrap_err();
            match e {
                MetaError::MetaRaftError(MetaRaftError::ForwardToLeader(ForwardToLeader {
                    leader_id: forward_leader_id,
                })) => {
                    assert_eq!(Some(leader_id), forward_leader_id);
                }
                _ => {
                    panic!("expect MetaRaftError::ForwardToLeader")
                }
            }
        }
    }

    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_snapshot_replication() -> anyhow::Result<()> {
    // - Bring up a cluster of 3.
    // - Write just enough logs to trigger a snapshot.
    // - Add a non-voter, test the snapshot is sync-ed
    // - Write logs to trigger another snapshot.
    // - Add

    // Create a snapshot every 10 logs
    let snap_logs = 10;

    let mut tc = MetaSrvTestContext::new(0);
    tc.config.raft_config.snapshot_logs_since_last = snap_logs;
    tc.config.raft_config.install_snapshot_timeout = 10_1000; // milli seconds. In a CI multi-threads test delays async task badly.
    tc.config.raft_config.max_applied_log_to_keep = 0;

    let mn = MetaNode::boot(&tc.config).await?;

    tc.assert_raft_server_connection().await?;

    mn.raft
        .wait(timeout())
        .state(State::Leader, "leader started")
        .await?;

    mn.raft
        .wait(timeout())
        .current_leader(0, "node-0 has leader")
        .await?;

    // initial membership, leader blank log, add node.
    let mut log_index = 2;

    mn.raft
        .wait(timeout())
        .log(Some(log_index), "leader init logs")
        .await?;

    let n_req = 12;

    for i in 0..n_req {
        let key = format!("test_meta_node_snapshot_replication-key-{}", i);
        mn.write(LogEntry {
            txid: None,
            time_ms: None,
            cmd: Cmd::UpsertKV(UpsertKV::update(&key, b"v")),
        })
        .await?;
    }
    log_index += n_req;

    info!("--- check the log is locally applied");

    mn.raft
        .wait(timeout())
        .log(Some(log_index), "applied on leader")
        .await?;

    info!("--- check the snapshot is created");

    mn.raft
        .wait(timeout())
        .metrics(
            |x| x.snapshot.map(|x| x.term) == Some(1) && x.snapshot.next_index() >= snap_logs,
            "snapshot is created by leader",
        )
        .await?;

    info!("--- start a non_voter to receive snapshot replication");

    let (_, tc1) = meta_node::start_meta_node_non_voter(mn.clone(), 1).await?;
    log_index += 1;

    let mn1 = tc1.meta_node();

    mn1.raft
        .wait(timeout())
        .log(Some(log_index), "non-voter replicated all logs")
        .await?;

    mn1.raft
        .wait(timeout())
        .metrics(
            |x| x.snapshot.map(|x| x.term) == Some(1) && x.snapshot.next_index() >= snap_logs,
            "snapshot is received by non-voter",
        )
        .await?;

    for i in 0..n_req {
        let key = format!("test_meta_node_snapshot_replication-key-{}", i);
        let got = mn1.get_kv(&key).await?;
        match got {
            None => {
                panic!("expect get some value for {}", key)
            }
            Some(SeqV { ref data, .. }) => {
                assert_eq!(data, b"v");
            }
        }
    }

    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_join() -> anyhow::Result<()> {
    // - Bring up a cluster
    // - Join a new node by sending a Join request to leader.
    // - Join a new node by sending a Join request to a non-voter.
    // - Restart all nodes and check if states are restored.

    let span = tracing::span!(tracing::Level::INFO, "test_meta_node_join");
    let _ent = span.enter();

    let (mut _nlog, mut tcs) =
        meta_node::start_meta_node_cluster(btreeset![0], btreeset![1]).await?;
    let mut all = test_context_nodes(&tcs);
    let tc0 = tcs.remove(0);
    let tc1 = tcs.remove(0);

    info!("--- bring up non-voter 2");

    let node_id = 2;
    let tc2 = MetaSrvTestContext::new(node_id);

    let mn2 = MetaNode::open_create_boot(&tc2.config.raft_config, None, Some(()), None).await?;

    info!("--- join non-voter 2 to cluster by leader");

    let leader_id = all[0].get_leader().await;
    let leader = all[leader_id as usize].clone();

    let admin_req = join_req(
        node_id,
        tc2.config.raft_config.raft_api_addr().await?,
        tc2.config.grpc_api_address.clone(),
        0,
    );
    leader.handle_forwardable_request(admin_req).await?;

    all.push(mn2.clone());

    info!("--- check all nodes has node-3 joined");
    {
        for mn in all.iter() {
            mn.raft
                .wait(timeout())
                .members(btreeset! {0,2}, format!("node-2 is joined: {}", mn.sto.id))
                .await?;
        }
    }

    info!("--- bring up non-voter 3");

    let node_id = 3;
    let tc3 = MetaSrvTestContext::new(node_id);
    let mn3 = MetaNode::open_create_boot(&tc3.config.raft_config, None, Some(()), None).await?;

    info!("--- join node-3 by sending rpc `join` to a non-leader");
    {
        let to_addr = tc1.config.raft_config.raft_api_addr().await?;

        let mut client = RaftServiceClient::connect(format!("http://{}", to_addr)).await?;
        let admin_req = join_req(
            node_id,
            tc3.config.raft_config.raft_api_addr().await?,
            tc3.config.grpc_api_address.clone(),
            1,
        );
        client.forward(admin_req).await?;
    }

    info!("--- check all nodes has node-3 joined");

    all.push(mn3.clone());
    for mn in all.iter() {
        mn.raft
            .wait(timeout())
            .members(
                btreeset! {0,2,3},
                format!("node-3 is joined: {}", mn.sto.id),
            )
            .await?;
    }

    info!("--- stop all meta node");

    for mn in all.drain(..) {
        mn.stop().await?;
    }

    info!("--- re-open all meta node");

    let mn0 = MetaNode::open_create_boot(&tc0.config.raft_config, Some(()), None, None).await?;
    let mn1 = MetaNode::open_create_boot(&tc1.config.raft_config, Some(()), None, None).await?;
    let mn2 = MetaNode::open_create_boot(&tc2.config.raft_config, Some(()), None, None).await?;
    let mn3 = MetaNode::open_create_boot(&tc3.config.raft_config, Some(()), None, None).await?;

    let all = vec![mn0, mn1, mn2, mn3];

    info!("--- check reopened memberships");

    for mn in all.iter() {
        mn.raft
            .wait(timeout())
            .members(btreeset! {0,2,3}, format!("node-{} membership", mn.sto.id))
            .await?;
    }

    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_leave() -> anyhow::Result<()> {
    // - Bring up a cluster
    // - Leave a node by sending a Leave request to a non-voter.
    // - Restart all nodes and check if states are restored.

    let (mut _nlog, tcs) =
        meta_node::start_meta_node_cluster(btreeset![0, 1, 2], btreeset![3]).await?;
    let mut all = test_context_nodes(&tcs);

    let leader_id = all[0].raft.metrics().borrow().current_leader.unwrap();
    let leader = all[leader_id as usize].clone();

    // leave a node
    let leave_node_id = 1;
    let admin_req = leave_req(leave_node_id, 0);
    leader.handle_forwardable_request(admin_req).await?;

    info!("--- stop all meta node");

    for mn in all.drain(..) {
        mn.stop().await?;
    }

    // restart the cluster and check membership
    info!("--- re-open all meta node");

    let tc0 = &tcs[0];
    let tc2 = &tcs[2];

    let mn0 = MetaNode::open_create_boot(&tc0.config.raft_config, Some(()), None, None).await?;
    let mn2 = MetaNode::open_create_boot(&tc2.config.raft_config, Some(()), None, None).await?;

    let all = vec![mn0, mn2];

    info!("--- check reopened memberships");

    for mn in all.iter() {
        mn.raft
            .wait(timeout())
            .members(btreeset! {0,2}, format!("node-{} membership", mn.sto.id))
            .await?;
    }

    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_join_rejoin() -> anyhow::Result<()> {
    // - Bring up a cluster
    // - Join a new node.
    // - Join another new node twice.

    let (mut _nlog, mut tcs) =
        meta_node::start_meta_node_cluster(btreeset![0], btreeset![]).await?;
    let mut all = test_context_nodes(&tcs);
    let _tc0 = tcs.remove(0);

    info!("--- bring up non-voter 1");

    let node_id = 1;
    let tc1 = MetaSrvTestContext::new(node_id);

    let mn1 = MetaNode::open_create_boot(&tc1.config.raft_config, None, Some(()), None).await?;

    info!("--- join non-voter 1 to cluster");

    let leader_id = all[0].get_leader().await;
    let leader = all[leader_id as usize].clone();
    let req = join_req(
        node_id,
        tc1.config.raft_config.raft_api_addr().await?,
        tc1.config.grpc_api_address,
        1,
    );
    leader.handle_forwardable_request(req).await?;

    all.push(mn1.clone());

    info!("--- check all nodes has node-1 joined");
    {
        for mn in all.iter() {
            mn.raft
                .wait(timeout())
                .members(btreeset! {0,1}, format!("node-1 is joined: {}", mn.sto.id))
                .await?;
        }
    }

    info!("--- bring up non-voter 3");

    let node_id = 2;
    let tc2 = MetaSrvTestContext::new(node_id);

    let mn2 = MetaNode::open_create_boot(&tc2.config.raft_config, None, Some(()), None).await?;

    info!("--- join node-2 by sending rpc `join` to a non-leader");
    {
        let req = join_req(
            node_id,
            tc2.config.raft_config.raft_api_addr().await?,
            tc2.config.grpc_api_address.clone(),
            1,
        );
        leader.handle_forwardable_request(req).await?;
    }
    info!("--- join node-2 again");
    {
        let req = join_req(
            node_id,
            tc2.config.raft_config.raft_api_addr().await?,
            tc2.config.grpc_api_address,
            1,
        );
        mn1.handle_forwardable_request(req).await?;
    }

    all.push(mn2.clone());

    info!("--- check all nodes has node-3 joined");

    for mn in all.iter() {
        mn.raft
            .wait(timeout())
            .members(
                btreeset! {0,1,2},
                format!("node-2 is joined: {}", mn.sto.id),
            )
            .await?;
    }

    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_restart() -> anyhow::Result<()> {
    // TODO check restarted follower.
    // - Start a leader and a non-voter;
    // - Restart them.
    // - Check old data an new written data.

    let (_nid0, tc0) = meta_node::start_meta_node_leader().await?;
    let mn0 = tc0.meta_node();

    let (_nid1, tc1) = meta_node::start_meta_node_non_voter(mn0.clone(), 1).await?;
    let mn1 = tc1.meta_node();

    let sto0 = mn0.sto.clone();
    let sto1 = mn1.sto.clone();

    let meta_nodes = vec![mn0.clone(), mn1.clone()];

    assert_upsert_kv_synced(meta_nodes.clone(), "key1").await?;

    // stop
    info!("shutting down all");

    let n = mn0.stop().await?;
    assert_eq!(3, n);
    let n = mn1.stop().await?;
    assert_eq!(3, n);

    info!("restart all");

    // restart
    let config = configs::Config::default();
    let mn0 = MetaNode::builder(&config.raft_config)
        .node_id(0)
        .sto(sto0)
        .build()
        .await?;
    let mn1 = MetaNode::builder(&config.raft_config)
        .node_id(1)
        .sto(sto1)
        .build()
        .await?;

    let meta_nodes = vec![mn0.clone(), mn1.clone()];

    mn0.raft
        .wait(timeout())
        .state(State::Leader, "leader restart")
        .await?;
    mn1.raft
        .wait(timeout())
        .state(State::Learner, "learner restart")
        .await?;
    mn1.raft
        .wait(timeout())
        .current_leader(0, "node-1 has leader")
        .await?;

    assert_upsert_kv_synced(meta_nodes.clone(), "key2").await?;

    // check old data
    assert_get_kv(meta_nodes, "key1", "key1").await?;

    Ok(())
}

#[async_entry::test(worker_threads = 5, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_restart_single_node() -> anyhow::Result<()> {
    // TODO(xp): This function will replace `test_meta_node_restart` after fs backed state machine is ready.

    // Test fs backed meta node restart.
    // - Start a cluster of a solo leader;
    // - Write one log.
    // - Restart.
    // - Check node state:
    //   - raft hard state
    //   - raft logs.
    //   - state machine:
    //     - Nodes
    //   - TODO(xp): snapshot is empty, since snapshot is not persisted in this version see `MetaStore`.
    // - Check cluster:
    //   - Leader is elected.
    //   - TODO(xp): Leader starts replication to follower and non-voter.
    //   - TODO(xp): New log will be successfully written and sync
    //   - TODO(xp): A new snapshot will be created and transferred  on demand.

    let mut log_index: u64 = 0;
    let (_id, tc) = meta_node::start_meta_node_leader().await?;
    // initial membeship, leader blank, add node
    log_index += 2;

    let want_hs;
    {
        let leader = tc.meta_node();

        leader
            .as_leader()
            .await?
            .write(LogEntry {
                txid: None,
                time_ms: None,
                cmd: Cmd::UpsertKV(UpsertKV::update("foo", b"1")),
            })
            .await?;
        log_index += 1;

        want_hs = leader.sto.raft_state.read_hard_state()?;

        leader.stop().await?;
    }

    info!("--- reopen MetaNode");

    let raft_conf = &tc.config.raft_config;

    let leader = MetaNode::open_create_boot(raft_conf, Some(()), None, None).await?;

    log_index += 1;

    leader
        .raft
        .wait(timeout())
        .state(State::Leader, "leader restart")
        .await?;
    leader
        .raft
        .wait(timeout())
        .log(
            Some(log_index),
            format!(
                "reopened: applied index at {} for node-{}",
                log_index, leader.sto.id
            ),
        )
        .await?;

    info!("--- check hard state");
    {
        let hs = leader.sto.raft_state.read_hard_state()?;
        assert_eq!(want_hs, hs);
    }

    info!("--- check logs");
    {
        let logs = leader.sto.log.range_values(..)?;
        info!("logs: {:?}", logs);
        assert_eq!(log_index as usize + 1, logs.len());
    }

    info!("--- check state machine: nodes");
    {
        let node = leader.sto.get_node(&0).await?.unwrap();
        assert_eq!(
            tc.config.raft_config.raft_api_advertise_host_endpoint(),
            node.endpoint
        );
    }

    Ok(())
}

#[async_entry::test(worker_threads = 3, init = "init_meta_ut!()", tracing_span = "debug")]
async fn test_meta_node_incr_seq() -> anyhow::Result<()> {
    let tc = MetaSrvTestContext::new(0);
    let addr = tc.config.raft_config.raft_api_addr().await?;

    let _mn = MetaNode::boot(&tc.config).await?;
    tc.assert_raft_server_connection().await?;

    let mut client = RaftServiceClient::connect(format!("http://{}", addr)).await?;

    let cases = common_meta_raft_store::state_machine::testing::cases_incr_seq();

    for (name, txid, k, want) in cases.iter() {
        let req = LogEntry {
            txid: txid.clone(),
            time_ms: None,
            cmd: Cmd::IncrSeq { key: k.to_string() },
        };
        let raft_reply = client.write(req).await?.into_inner();

        let res: Result<AppliedState, RetryableError> = raft_reply.into();
        let resp: AppliedState = res?;
        match resp {
            AppliedState::Seq { seq } => {
                assert_eq!(*want, seq, "{}", name);
            }
            _ => {
                panic!("not Seq")
            }
        }
    }

    Ok(())
}

fn join_req(
    node_id: NodeId,
    endpoint: Endpoint,
    grpc_api_addr: String,
    forward: u64,
) -> ForwardRequest {
    ForwardRequest {
        forward_to_leader: forward,
        body: ForwardRequestBody::Join(JoinRequest {
            node_id,
            endpoint,
            grpc_api_addr,
        }),
    }
}

fn leave_req(node_id: NodeId, forward: u64) -> ForwardRequest {
    ForwardRequest {
        forward_to_leader: forward,
        body: ForwardRequestBody::Leave(LeaveRequest { node_id }),
    }
}

/// Write one log on leader, check all nodes replicated the log.
/// Returns the number log committed.
async fn assert_upsert_kv_synced(meta_nodes: Vec<Arc<MetaNode>>, key: &str) -> anyhow::Result<u64> {
    let leader_id = meta_nodes[0].get_leader().await;
    let leader = meta_nodes[leader_id as usize].clone();

    let last_applied = leader.raft.metrics().borrow().last_applied;
    info!("leader: last_applied={:?}", last_applied);
    {
        leader
            .as_leader()
            .await?
            .write(LogEntry {
                txid: None,
                time_ms: None,
                cmd: Cmd::UpsertKV(UpsertKV::update(key, key.as_bytes())),
            })
            .await?;
    }

    // Assert applied index on every node
    for (_i, mn) in meta_nodes.iter().enumerate() {
        mn.raft
            .wait(timeout())
            .log(
                Some(last_applied.next_index()),
                format!(
                    "check upsert-kv has applied index at {} for node-{}",
                    last_applied.next_index(),
                    mn.sto.id
                ),
            )
            .await?;
    }
    assert_get_kv(meta_nodes.clone(), key, key).await?;

    Ok(1)
}

async fn assert_get_kv(
    meta_nodes: Vec<Arc<MetaNode>>,
    key: &str,
    value: &str,
) -> anyhow::Result<()> {
    for (i, mn) in meta_nodes.iter().enumerate() {
        let got = mn.get_kv(key).await?;
        assert_eq!(
            value.to_string().into_bytes(),
            got.unwrap().data,
            "n{} applied value",
            i
        );
    }
    Ok(())
}

fn test_context_nodes(tcs: &[MetaSrvTestContext]) -> Vec<Arc<MetaNode>> {
    tcs.iter().map(|tc| tc.meta_node()).collect::<Vec<_>>()
}
