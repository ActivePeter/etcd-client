mod cache;
/// Etcd delete mod for delete operations.
mod delete;
/// Etcd get mod for get operations.
mod get;
/// Etcd put mod for put operations.
mod put;
/// Etcd range mod for range fetching operations.
mod range;
/// Etcd txn mod for transaction operations.
mod txn;

pub use super::watch::{EtcdWatchRequest, EtcdWatchResponse};
pub use cache::Cache;
pub use delete::{EtcdDeleteRequest, EtcdDeleteResponse};
pub use get::{EtcdGetRequest, EtcdGetResponse};
pub use put::{EtcdPutRequest, EtcdPutResponse};
pub use range::{EtcdRangeRequest, EtcdRangeResponse};
pub use txn::{EtcdTxnRequest, EtcdTxnResponse, TxnCmp, TxnOpResponse};

use super::OverflowArithmetic;
use crate::protos::kv::Event_EventType;
use crate::protos::rpc::RangeResponse;
use crate::protos::rpc_grpc::{KvClient, WatchClient};
use crate::retryable;
use crate::CURRENT_INTERVAL_ENV_KEY;
use crate::CURRENT_INTERVAL_VALUE;
use crate::INITIAL_INTERVAL_ENV_KEY;
use crate::INITIAL_INTERVAL_VALUE;
use crate::MAX_ELAPSED_TIME_ENV_KEY;
use crate::MAX_ELAPSED_TIME_VALUE;
use backoff::ExponentialBackoff;
use futures::stream::StreamExt;
use grpcio::WriteFlags;
use log::warn;
use protobuf::RepeatedField;
use smol::channel::{bounded, unbounded, Sender};
use std::str;
use std::time::Duration;

use crate::protos::kv::KeyValue;
use crate::Result as Res;
use futures::SinkExt;
use utilities::Cast;

/// Key-Value client.
#[derive(Clone)]
pub struct Kv {
    /// Etcd Key-Value client.
    client: KvClient,
    /// Etcd client cache.
    cache: Cache,
    /// Enable etcd client cache.
    cache_enable: bool,
    /// Etcd watch request sender.
    watch_sender: Sender<EtcdWatchRequest>,
}

/// Etcd client cache default size.
const ETCD_CACHE_DEFAULT_SIZE: usize = 64;

impl Kv {
    /// Creates a new `KvClient`.
    ///
    /// This method should only be called within etcd client.
    pub(crate) fn new(
        client: KvClient,
        watch_client: &WatchClient,
        cache_size: usize,
        cache_enable: bool,
    ) -> Self {
        let etcd_cache_size = if cache_size == 0 {
            ETCD_CACHE_DEFAULT_SIZE
        } else {
            cache_size
        };

        let (watch_req_sender, watch_req_receiver) = unbounded::<EtcdWatchRequest>();
        let (watch_id_sender, watch_id_receiver) = bounded::<i64>(1);

        let cache = Cache::new(etcd_cache_size, watch_req_sender.clone());
        let (mut client_req_sender, mut client_resp_receiver) = watch_client
            .watch()
            .unwrap_or_else(|e| panic!("failed to send watch commend, the response is: {}", e));

        let cache_clone = cache.clone();
        let cache_inner = cache.clone();

        // Task that handles all the pending watch requests.
        smol::spawn(async move {
            while let Ok(watch_req) = watch_req_receiver.recv().await {
                let processing_key = watch_req.get_key();
                client_req_sender
                    .send((watch_req.clone().into(), WriteFlags::default()))
                    .await
                    .unwrap_or_else(|e| panic!("Fail to send watch request, the error is: {}", e));
                // Wait until etcd server returns watch id.
                let watch_id = watch_id_receiver.recv().await.unwrap_or_else(|e| {
                    panic!("Fail to receive watch id from channel, the error is {}", e)
                });
                // Watch request can only be create or cancel.
                if watch_req.is_create() {
                    cache_clone.insert_watch_id(processing_key.clone(), watch_id);
                } else {
                    cache_clone.delete_watch_id(&processing_key);
                }
            }
        })
        .detach();

        // Task that handle the watch responses from Etcd server.
        smol::spawn(async move {
            while let Some(watch_resp) = client_resp_receiver.next().await {
                match watch_resp {
                    Ok(resp) => {
                        // TODO: Check if need to spawn new task here.
                        if resp.get_created() || resp.get_canceled() {
                            watch_id_sender
                                .send(resp.get_watch_id())
                                .await
                                .unwrap_or_else(|e| {
                                    panic!("Fail to send watch id, the error is {}", e)
                                });
                        } else {
                            let events = resp.get_events().to_vec();
                            for event in events {
                                if event.get_field_type() == Event_EventType::PUT {
                                    if let Some(valid_value) =
                                        cache_inner.search(event.get_kv().get_key().to_vec()).await
                                    {
                                        // Only update the cache if event's version is larger than
                                        // existing value's version
                                        if valid_value.version < event.get_kv().get_version() {
                                            cache_inner
                                                .insert(
                                                    event.get_kv().get_key().to_vec(),
                                                    event.get_kv().clone(),
                                                )
                                                .await;
                                        }
                                    }
                                } else {
                                    cache_inner
                                        .delete(event.get_kv().get_key().to_vec(), true)
                                        .await;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Watch response contains error, the error is: {}", e);
                        break;
                    }
                }
            }
        })
        .detach();
        Self {
            client,
            cache,
            cache_enable,
            watch_sender: watch_req_sender,
        }
    }

    /// Performs a key-value saving operation.
    ///
    /// # Errors
    ///
    /// Will return `Err` if RPC call is failed.
    #[inline]
    pub async fn put(&mut self, req: EtcdPutRequest) -> Res<EtcdPutResponse> {
        let key = req.get_key();
        let resp: EtcdPutResponse = retryable!(|| async {
            let resp = self.client.put_async(&req.clone().into())?;
            Ok(From::from(resp.await?))
        });

        if self.cache.search(key.clone()).await == None {
            let revision = resp.get_revision();
            // Creates a new watch request and adds to the send queue.
            let mut watch_request = EtcdWatchRequest::create(KeyRange::key(key));
            watch_request.set_start_revision(revision.cast());
            self.watch_sender
                .send(watch_request)
                .await
                .unwrap_or_else(|e| panic!("Fail to send watch request, the error is {}", e));
        }
        Ok(resp)
    }

    /// Performs a single key-value fetching operation.
    ///
    /// # Errors
    ///
    /// Will return `Err` if RPC call is failed.
    #[inline]
    pub async fn get(&mut self, req: EtcdGetRequest) -> Res<EtcdGetResponse> {
        if self.cache_enable {
            if let Some(value) = self.cache.search(req.get_key()).await {
                let mut response = RangeResponse::new();
                response.set_count(1);
                response.set_kvs(RepeatedField::from_vec(vec![value]));
                return Ok(EtcdGetResponse::new(response));
            }
        }

        let resp = retryable!(|| async {
            let resp = self.client.range_async(&req.clone().into())?;
            Ok(resp.await?)
        });
        if self.cache_enable {
            let kvs = resp.get_kvs();
            for kv in kvs {
                if self.cache.search(kv.get_key().to_vec()).await == None {
                    // Creates a new watch request and adds to the send queue.
                    let watch_request = EtcdWatchRequest::create(KeyRange::key(kv.get_key()));
                    self.watch_sender
                        .send(watch_request)
                        .await
                        .unwrap_or_else(|e| {
                            panic!("Fail to send watch request, the error is {}", e)
                        });
                }
                self.cache.insert(kv.get_key().to_vec(), kv.clone()).await;
            }
        }
        Ok(From::from(resp))
    }

    /// Performs a range key-value fetching operation.
    ///
    /// # Errors
    ///
    /// Will return `Err` if RPC call is failed.
    #[inline]
    pub async fn range(&mut self, req: EtcdRangeRequest) -> Res<EtcdRangeResponse> {
        let resp = retryable!(|| async {
            let resp = self.client.range_async(&req.clone().into())?;
            Ok(From::from(resp.await?))
        });
        Ok(resp)
    }

    /// Performs a key-value deleting operation.
    ///
    /// # Errors
    ///
    /// Will return `Err` if RPC call is failed.
    #[inline]
    pub async fn delete(&mut self, req: EtcdDeleteRequest) -> Res<EtcdDeleteResponse> {
        let resp = retryable!(|| async {
            let resp = self.client.delete_range_async(&req.clone().into())?;
            Ok(From::from(resp.await?))
        });
        Ok(resp)
    }

    /// Performs a transaction operation.
    ///
    /// # Errors
    ///
    /// Will return `Err` if RPC call is failed.
    #[inline]
    pub async fn txn(&mut self, req: EtcdTxnRequest) -> Res<EtcdTxnResponse> {
        let resp = retryable!(|| async {
            let resp = self.client.txn_async(&req.clone().into())?;
            Ok(From::from(resp.await?))
        });
        Ok(resp)
    }
}

/// Key-Value pair.
#[derive(Clone, PartialEq)]
pub struct EtcdKeyValue {
    /// Etcd `KeyValue` pairs struct.
    proto: KeyValue,
}

impl EtcdKeyValue {
    /// Gets the key in bytes. An empty key is not allowed.
    #[inline]
    pub fn key(&self) -> &[u8] {
        &self.proto.key
    }

    /// Takes the key out of response, leaving an empty vector in its place.
    #[inline]
    pub fn take_key(&mut self) -> Vec<u8> {
        std::mem::replace(&mut self.proto.key, vec![])
    }

    /// Converts the key from bytes `&[u8]` to `&str`.
    /// Leaves the original `&[u8]` in place, and creates a new string slice containing the entire content.
    #[inline]
    pub fn key_str(&self) -> &str {
        std::str::from_utf8(&self.proto.key)
            .unwrap_or_else(|e| panic!("Fail to convert bytes to string, the error is: {}", e))
    }

    /// Gets the value held by the key, in bytes.
    #[inline]
    pub fn value(&self) -> &[u8] {
        &self.proto.value
    }

    /// Takes the value out of response, leaving an empty vector in its place.
    #[inline]
    pub fn take_value(&mut self) -> Vec<u8> {
        std::mem::replace(&mut self.proto.value, vec![])
    }

    /// Converts the value from bytes `&[u8]` to `&str`.
    /// Leaves the original `&[u8]` in place, and creates a new string slice containing the entire content.
    #[inline]
    pub fn value_str(&self) -> &str {
        std::str::from_utf8(&self.proto.value)
            .unwrap_or_else(|e| panic!("Fail to convert bytes to string, the error is {}", e))
    }

    /// Gets the revision of last creation on this key.
    #[inline]
    pub fn create_revision(&self) -> usize {
        self.proto.create_revision.cast()
    }

    /// Gets the revision of last modification on this key.
    #[inline]
    pub fn mod_revision(&self) -> usize {
        self.proto.mod_revision.cast()
    }

    /// Gets the version of the key.
    #[inline]
    pub fn version(&self) -> usize {
        self.proto.version.cast()
    }

    /// Gets the ID of the lease that attached to key.
    #[inline]
    pub fn lease(&self) -> usize {
        self.proto.lease.cast()
    }

    /// Returns `true` if this `KeyValue` has a lease attached, and `false` otherwise.
    #[inline]
    pub const fn has_lease(&self) -> bool {
        self.proto.lease != 0
    }
}

impl From<KeyValue> for EtcdKeyValue {
    #[inline]
    fn from(kv: KeyValue) -> Self {
        Self { proto: kv }
    }
}

/// `KeyRange` is an abstraction for describing etcd key of various types.
pub struct KeyRange {
    /// The first key of the range and should be non-empty
    key: Vec<u8>,
    /// The key following the last key of the range
    range_end: Vec<u8>,
}

impl KeyRange {
    /// Creates a new `KeyRange` for describing a range of multiple keys.
    #[inline]
    pub fn range<K, R>(key: K, range_end: R) -> Self
    where
        K: Into<Vec<u8>>,
        R: Into<Vec<u8>>,
    {
        Self {
            key: key.into(),
            range_end: range_end.into(),
        }
    }

    /// Creates a new `KeyRange` for describing a specified key.
    #[inline]
    pub fn key<K>(key: K) -> Self
    where
        K: Into<Vec<u8>>,
    {
        Self {
            key: key.into(),
            range_end: vec![],
        }
    }

    /// Creates a new `KeyRange` for describing all keys.
    #[inline]
    #[must_use]
    pub fn all() -> Self {
        Self {
            key: vec![0],
            range_end: vec![0],
        }
    }

    /// Creates a new `KeyRange` for describing keys prefixed with specified value.
    #[inline]
    pub fn prefix<K>(prefix: K) -> Self
    where
        K: Into<Vec<u8>>,
    {
        let key = prefix.into();
        if key.is_empty() {
            // An empty Vec<u8> results in an invalid KeyRange.
            // Assume that an empty value passed to this method implies no prefix (i.e., all keys).
            return Self::all();
        }

        let mut first_value = true;
        let mut range_end = key
            .iter()
            .rev()
            .filter_map(|e| {
                if *e < 0xff {
                    if first_value {
                        first_value = false;
                        Some(e.overflow_add(1))
                    } else {
                        Some(*e)
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<u8>>();
        range_end.reverse();
        Self { key, range_end }
    }

    /// Take key value
    #[inline]
    pub fn take_key(&mut self) -> Vec<u8> {
        std::mem::replace(&mut self.key, vec![])
    }

    /// Take `range_end` value
    #[inline]
    pub fn take_range_end(&mut self) -> Vec<u8> {
        std::mem::replace(&mut self.range_end, vec![])
    }
}
