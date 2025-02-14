use cid::Cid;
use futures::sink::unfold;
use rhizome::{
    kernel::math,
    runtime::{client::Client, reactor::Reactor},
    tuple::{InputTuple, Tuple},
    types::Any,
    value::Val,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    select,
};
use tokio_stream::StreamExt;
use tokio_util::codec::{Framed, LinesCodec};

use futures::SinkExt;
use std::{
    collections::HashMap,
    env,
    error::Error,
    sync::{Arc, RwLock},
};

#[derive(Debug, Default)]
struct Database {
    bs: RwLock<HashMap<Cid, InputTuple>>,
    map: RwLock<HashMap<String, (Cid, String)>>,
}

enum Request {
    Get { key: String },
    Set { key: String, val: String },
    Pull { addr: String },
    List,
    Dump,
}

enum Response {
    Val {
        key: String,
        val: String,
    },
    Set {
        key: String,
        val: String,
        previous: Option<String>,
        cid: Cid,
    },
    Pull {
        entries: Vec<Cid>,
    },
    Dump {
        entries: Vec<InputTuple>,
    },
    List {
        entries: Vec<(String, String)>,
    },
    Error {
        msg: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());

    let listener = TcpListener::bind(&addr).await?;
    println!("Listening on: {}", addr);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<InputTuple>(1);
    let db = Arc::new(Database::default());

    let (mut client, mut client_rx, reactor) = Client::new();

    tokio::spawn(async move { run_reactor(reactor).await });
    tokio::spawn({
        let db = Arc::clone(&db);
        async move {
            client
                .register_sink(
                    "head",
                    Box::new({
                        let db = Arc::clone(&db);

                        || {
                            Box::new(unfold(db, move |db, fact: Tuple| async move {
                                let Some(Val::Cid(cid)) = fact.col(&"cid".into()) else {
                                    panic!("cid is not a cid");
                                };

                                let Val::String(key) = fact.col(&"key".into()).unwrap() else {
                                    panic!("key is not a string");
                                };

                                let Val::String(val) = fact.col(&"val".into()).unwrap() else {
                                    panic!("val is not a string");
                                };

                                db.map
                                    .write()
                                    .unwrap()
                                    .insert(key.to_string(), (cid, val.to_string()));

                                Ok(db)
                            }))
                        }
                    }),
                )
                .await
                .unwrap();

            loop {
                select! {
                    command = rx.recv() => if let Some(fact) = command {
                        db.bs.write().unwrap().insert(fact.cid().unwrap(), fact.clone());

                        client.insert_fact(fact).await.unwrap();
                    }
                }
            }
        }
    });

    tokio::spawn(async move {
        loop {
            select! {
                _ = client_rx.next() => {
                    continue;
                }
            }
        }
    });

    loop {
        match listener.accept().await {
            Ok((socket, _)) => {
                let db = Arc::clone(&db);
                let tx = tx.clone();

                tokio::spawn(async move {
                    let mut lines = Framed::new(socket, LinesCodec::new());

                    while let Some(result) = lines.next().await {
                        match result {
                            Ok(line) => {
                                let response = handle_request(&line, &db, &tx).await;

                                let response = response.serialize();

                                if let Err(e) = lines.send(response.as_str()).await {
                                    println!("error on sending response; error = {:?}", e);
                                }
                            }
                            Err(e) => {
                                println!("error on decoding from socket; error = {:?}", e);
                            }
                        }
                    }
                });
            }
            Err(e) => println!("error accepting socket; error = {:?}", e),
        }
    }
}

async fn handle_request(
    line: &str,
    db: &Database,
    tx: &tokio::sync::mpsc::Sender<InputTuple>,
) -> Response {
    let request = match Request::parse(line) {
        Ok(req) => req,
        Err(e) => return Response::Error { msg: e },
    };

    match request {
        Request::Get { key } => match db.map.read().unwrap().get(&key) {
            Some((_, val)) => Response::Val {
                key,
                val: val.clone(),
            },
            None => Response::Error {
                msg: format!("no key {}", key),
            },
        },
        Request::Set { key, val } => {
            let previous = db.map.read().unwrap().get(&key).cloned();

            match previous {
                Some((parent, previous)) => {
                    let fact = InputTuple::new("kv", key.clone(), val.clone(), vec![parent]);
                    let cid = fact.cid().unwrap();

                    tx.send(fact).await.unwrap();

                    Response::Set {
                        key,
                        val,
                        previous: Some(previous),
                        cid,
                    }
                }
                None => {
                    let fact = InputTuple::new("kv", key.clone(), val.clone(), vec![]);
                    let cid = fact.cid().unwrap();

                    tx.send(fact).await.unwrap();

                    Response::Set {
                        key,
                        val,
                        previous: None,
                        cid,
                    }
                }
            }
        }
        Request::Pull { addr } => {
            let stream = TcpStream::connect(addr).await.unwrap();
            let mut stream = BufReader::new(stream);

            stream.write_all(b"DUMP\n").await.unwrap();

            let mut buf = String::new();
            let facts: Vec<InputTuple> = match stream.read_line(&mut buf).await {
                Ok(_) => serde_json::from_str(&buf).unwrap(),
                _ => panic!(),
            };
            for fact in &facts {
                tx.send(fact.clone()).await.unwrap();
            }

            let entries = facts.into_iter().map(|f| f.cid().unwrap()).collect();

            Response::Pull { entries }
        }
        Request::List => {
            let entries = db
                .map
                .read()
                .unwrap()
                .iter()
                .map(|(k, (_, v))| (k.clone(), v.clone()))
                .collect();

            Response::List { entries }
        }
        Request::Dump => {
            let entries = db.bs.read().unwrap().values().cloned().collect();

            Response::Dump { entries }
        }
    }
}

impl Request {
    fn parse(input: &str) -> Result<Request, String> {
        let mut parts = input.splitn(3, ' ');
        match parts.next() {
            Some("GET") => {
                let key = parts.next().ok_or("GET must be followed by a key")?;
                if parts.next().is_some() {
                    return Err("GET's key must not be followed by anything".into());
                }
                Ok(Request::Get {
                    key: key.to_string(),
                })
            }
            Some("SET") => {
                let key = match parts.next() {
                    Some(key) => key,
                    None => return Err("SET must be followed by a key".into()),
                };
                let val = match parts.next() {
                    Some(val) => val,
                    None => return Err("SET needs a value".into()),
                };
                Ok(Request::Set {
                    key: key.to_string(),
                    val: val.to_string(),
                })
            }
            Some("PULL") => {
                let addr = match parts.next() {
                    Some(addr) => addr,
                    None => return Err("PULL must be followed by an address".into()),
                };
                Ok(Request::Pull {
                    addr: addr.to_string(),
                })
            }
            Some("LIST") => Ok(Request::List),
            Some("DUMP") => Ok(Request::Dump),
            Some(cmd) => Err(format!("unknown command: {}", cmd)),
            None => Err("empty input".into()),
        }
    }
}

impl Response {
    fn serialize(&self) -> String {
        match *self {
            Response::Val { ref key, ref val } => format!("{} = {}", key, val),
            Response::Set {
                ref key,
                ref val,
                ref previous,
                ref cid,
            } => format!(
                "set {} = `{}`, previous: {:?}, cid: {}",
                key, val, previous, cid
            ),
            Response::Pull { ref entries } => entries
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            Response::List { ref entries } => {
                let mut result = vec![];
                for (key, val) in entries {
                    result.push(format!("{} = {}", key, val));
                }
                result.join("\n")
            }
            Response::Dump { ref entries } => serde_json::to_string(&entries).unwrap(),
            Response::Error { ref msg } => format!("error: {}", msg),
        }
    }
}

async fn run_reactor(reactor: Reactor) {
    reactor
        .async_run(|p| {
            p.output("set", |h| {
                h.column::<Cid>("cid")
                    .column::<i32>("store")
                    .column::<Any>("key")
                    .column::<Any>("val")
            })?;

            p.output("root", |h| {
                h.column::<Cid>("cid")
                    .column::<i32>("store")
                    .column::<Any>("key")
            })?;

            p.output("child", |h| h.column::<Cid>("cid").column::<Cid>("parent"))?;
            p.output("latestSibling", |h| h.column::<Cid>("cid"))?;

            p.output("head", |h| {
                h.column::<Cid>("cid")
                    .column::<i32>("store")
                    .column::<Any>("key")
                    .column::<Any>("val")
            })?;

            p.rule::<(Cid, i32, Any, Any)>("set", &|h, b, (cid, store, k, v)| {
                h.bind((("cid", cid), ("store", store), ("key", k), ("val", v)))?;
                b.search_cid(
                    "evac",
                    cid,
                    (("entity", store), ("attribute", k), ("value", v)),
                )?;

                Ok(())
            })?;

            p.rule::<(Cid, i32, Any)>("root", &|h, b, (cid, store, k)| {
                h.bind((("cid", cid), ("store", store), ("key", k)))?;

                b.search("set", (("cid", cid), ("store", store), ("key", k)))?;
                b.except("links", (("from", cid),))?;

                Ok(())
            })?;

            p.rule::<(Cid, Cid, i32, Any)>("child", &|h, b, (cid, parent, store, k)| {
                h.bind((("cid", cid), ("parent", parent)))?;

                b.search("set", (("cid", cid), ("store", store), ("key", k)))?;
                b.search("links", (("from", cid), ("to", parent)))?;
                b.search("root", (("cid", parent), ("store", store), ("key", k)))?;

                Ok(())
            })?;

            p.rule::<(Cid, Cid, i32, Any)>("child", &|h, b, (cid, parent, store, k)| {
                h.bind((("cid", cid), ("parent", parent)))?;

                b.search("set", (("cid", cid), ("store", store), ("key", k)))?;
                b.search("links", (("from", cid), ("to", parent)))?;
                b.search("child", (("cid", parent),))?;

                Ok(())
            })?;

            p.rule::<(Cid, i32, Any)>("latestSibling", &|h, b, (cid, store, k)| {
                h.bind((("cid", cid),))?;

                b.search("root", (("store", store), ("key", k)))?;
                b.group_by(
                    cid,
                    "root",
                    (("cid", cid), ("store", store), ("key", k)),
                    math::min(cid),
                )?;

                Ok(())
            })?;

            p.rule::<(Cid, Cid)>("latestSibling", &|h, b, (cid, parent)| {
                h.bind((("cid", cid),))?;

                b.search("latestSibling", (("cid", parent),))?;
                b.group_by(
                    cid,
                    "child",
                    (("cid", cid), ("parent", parent)),
                    math::min(cid),
                )?;

                Ok(())
            })?;

            p.rule::<(Cid, i32, Any, Any)>("head", &|h, b, (cid, store, k, v)| {
                h.bind((("cid", cid), ("store", store), ("key", k), ("val", v)))?;

                b.search("latestSibling", (("cid", cid),))?;
                b.search(
                    "set",
                    (("cid", cid), ("store", store), ("key", k), ("val", v)),
                )?;
                b.except("child", (("parent", cid),))?;

                Ok(())
            })?;

            Ok(p)
        })
        .await
        .unwrap()
}
