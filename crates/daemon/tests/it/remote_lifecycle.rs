//! A box connection across client daemon restarts, and a reconnect that needs
//! a person to sign in. A child of `remote`, whose box and client harness it
//! reuses.

use std::{net::Ipv6Addr, sync::atomic::Ordering};

use bytes::Bytes;
use devlanes_daemon::{Handler, Session};
use devlanes_ipc::Frame;
use devlanes_proto::{Context, Control, Method, Request, remote::PROMPT_EVENT};
use devlanes_state::Store;
use serde_json::json;
use tokio::net::TcpStream;

use super::{BOX, BoxSide, ClientSide, PASSWORD, SOON, box_port, echo, http_get, ipv6_loopback};
use crate::{LIMIT, cx, eventually, repo, temp};

// Port ranges: these tests take 21500-21999, which no other test in this
// binary uses (see `remote_boxes.rs`).

#[tokio::test(flavor = "multi_thread")]
/// A box connected before a client daemon restart is connected again by the
/// next daemon, as a box that connected before, without any `connect`. After
/// `disconnect` it is no longer wanted, so the daemon after that leaves it.
async fn a_restart_restores_wanted_boxes() {
    if !ipv6_loopback().await {
        eprintln!("skipping a_restart_restores_wanted_boxes: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "boxrepo", BOX);
        let side = BoxSide::start(&tmp, "box", "21500-21624").await;
        side.h
            .call(Method::Start, json!({"units": ["echo"]}), cx(&main))
            .await
            .expect("box start");
        let echo_port = box_port(&side.h, 0, "echo").await;

        let client = ClientSide::start(&tmp, "client", "21625-21749", &side).await;
        client.connect("box").await;
        let mirrored = eventually(SOON, || async {
            client.mirrored().await.contains(&echo_port).then_some(())
        })
        .await;
        assert!(mirrored.is_some(), "echo mirrored before the restart");
        assert_eq!(*client.handed_out.lock().expect("handed out"), [false]);
        client.stop().await;

        let client = ClientSide::start(&tmp, "client", "21625-21749", &side).await;
        let restored = eventually(SOON, || async {
            (client.host().await["state"] == "connected" && echo(echo_port).await).then_some(())
        })
        .await;
        assert!(
            restored.is_some(),
            "the restarted daemon reconnected the box: {}",
            client.host().await
        );
        assert_eq!(
            *client.handed_out.lock().expect("handed out"),
            [true],
            "a restored box counts as connected before"
        );

        client
            .h
            .call(
                Method::Disconnect,
                json!({"host": "box"}),
                Context::default(),
            )
            .await
            .expect("disconnect");
        let home = client.h.home.clone();
        client.stop().await;
        let store = Store::open(home.join("state.db")).await.expect("store");
        let hosts = store.remote_hosts().await.expect("hosts");
        assert_eq!(hosts.len(), 1, "{hosts:?}");
        assert!(!hosts[0].wanted, "a disconnected box is not restored");
        store.close().await.expect("close");

        side.stop().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A reconnect whose prompt has no CLI to answer it waits for a sign-in:
/// `remote ls` and `status` show `needs_auth` with the command to run, the
/// listeners stay and the route serves the sign-in page. `remote connect`
/// with a CLI that answers resumes the link.
async fn an_unanswered_prompt_waits_for_sign_in() {
    if !ipv6_loopback().await {
        eprintln!("skipping an_unanswered_prompt_waits_for_sign_in: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "boxrepo", BOX);
        let side = BoxSide::start(&tmp, "box", "21750-21874").await;
        side.h
            .call(Method::Start, json!({"units": ["web", "echo"]}), cx(&main))
            .await
            .expect("box start");
        let echo_port = box_port(&side.h, 0, "echo").await;
        let client = ClientSide::start(&tmp, "client", "21875-21999", &side).await;
        client.connect("box").await;
        let served = eventually(SOON, || async {
            http_get(client.proxy_port, "web.boxapp.localhost")
                .await
                .filter(|response| response.contains("hello-from-box"))
        })
        .await;
        assert!(served.is_some(), "web is routed to the box");

        // The box now asks for a password, and no CLI is attached to answer.
        client.sign_in.store(true, Ordering::SeqCst);
        side.transport.last_link().expect("link").sever();
        let host = eventually(SOON, || async {
            let host = client.host().await;
            (host["state"] == "needs_auth").then_some(host)
        })
        .await;
        let host = match host {
            Some(host) => host,
            None => panic!("the link needs a sign-in: {}", client.host().await),
        };
        assert_eq!(host["fix"], "devlanes remote connect box", "{host}");
        let error = host["error"].as_str().unwrap_or_default();
        assert!(error.contains("Permission denied"), "{host}");
        let status = client
            .h
            .call(Method::Status, json!({"all": true}), Context::default())
            .await
            .expect("status");
        assert_eq!(status["hosts"][0]["state"], "needs_auth", "{status}");
        assert_eq!(
            status["hosts"][0]["fix"], "devlanes remote connect box",
            "{status}"
        );

        assert!(
            TcpStream::connect((Ipv6Addr::LOCALHOST, echo_port))
                .await
                .is_ok(),
            "the listener stays while the box waits for a sign-in"
        );
        let route = eventually(SOON, || async {
            let state = client.route_state("web.boxapp.localhost").await;
            (state.as_deref() == Some("sign_in_needed")).then_some(())
        })
        .await;
        assert!(route.is_some(), "the route waits for a sign-in");
        let page = http_get(client.proxy_port, "web.boxapp.localhost")
            .await
            .unwrap_or_default();
        assert!(page.contains("box-sign-in-needed"), "{page}");
        assert!(page.contains("devlanes remote connect box"), "{page}");

        // `remote connect` with a CLI attached: its prompt is answered and the
        // link connects again.
        let (session, mut frames) = Session::local(9_301);
        let engine = client.h.engine.clone();
        let attached = session.clone();
        let mut connecting = tokio::spawn(async move {
            let request = Request {
                id: 1,
                method: Method::Connect,
                params: json!({"host": "box", "background": true}),
                context: Context::default(),
            };
            engine.handle(request, attached).await
        });
        let engine = client.h.engine.clone();
        let mut prompts = 0;
        let connected = loop {
            tokio::select! {
                result = &mut connecting => break result.expect("connect task"),
                frame = frames.recv() => match frame {
                    None => break (&mut connecting).await.expect("connect task"),
                    Some(Frame::Control(Control::Event(event))) if event.event == PROMPT_EVENT => {
                        prompts += 1;
                        let stream = event.data["stream_id"].as_u64().expect("stream id");
                        engine
                            .input(stream, Bytes::from_static(PASSWORD.as_bytes()), session.clone())
                            .await
                            .expect("answer");
                    }
                    Some(_) => {}
                },
            }
        };
        let connected = connected.expect("connect resumes the link");
        assert_eq!(connected["state"], "connected", "{connected}");
        assert_eq!(prompts, 1, "one prompt for the resumed attempt");
        let back = eventually(SOON, || async {
            http_get(client.proxy_port, "web.boxapp.localhost")
                .await
                .filter(|response| response.contains("hello-from-box"))
        })
        .await;
        assert!(back.is_some(), "web is served again");
        assert!(echo(echo_port).await, "the mirrored port works again");

        session.close();
        client.stop().await;
        side.stop().await;
    })
    .await
    .expect("timeout");
}
