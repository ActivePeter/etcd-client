use std::time::Duration;

use crate::proto::etcdserverpb;
use crate::ResponseHeader;

/// Request for granting lease.
pub struct LeaseGrantRequest {
    proto: etcdserverpb::LeaseGrantRequest,
}

impl LeaseGrantRequest {
    /// Creates a new LeaseGrantRequest with the specified TTL.
    pub fn new(ttl: Duration) -> Self {
        let proto = etcdserverpb::LeaseGrantRequest {
            ttl: ttl.as_secs() as i64,
            id: 0,
        };

        Self { proto }
    }

    /// Set custom lease ID.
    pub fn set_id(&mut self, id: u64) {
        self.proto.id = id as i64
    }
}

impl Into<etcdserverpb::LeaseGrantRequest> for LeaseGrantRequest {
    fn into(self) -> etcdserverpb::LeaseGrantRequest {
        self.proto
    }
}

#[derive(Debug)]
pub struct LeaseGrantResponse {
    proto: etcdserverpb::LeaseGrantResponse,
}

impl LeaseGrantResponse {
    /// Takes the header out of response, leaving a `None` in its place.
    pub fn take_header(&mut self) -> Option<ResponseHeader> {
        match self.proto.header.take() {
            Some(header) => Some(From::from(header)),
            _ => None,
        }
    }

    /// Gets the lease ID for the granted lease.
    pub fn id(&self) -> u64 {
        self.proto.id as u64
    }

    /// Gets the server chosen lease time-to-live in seconds.
    pub fn ttl(&self) -> u64 {
        self.proto.ttl as u64
    }
}

impl From<etcdserverpb::LeaseGrantResponse> for LeaseGrantResponse {
    fn from(resp: etcdserverpb::LeaseGrantResponse) -> Self {
        Self { proto: resp }
    }
}
