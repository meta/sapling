/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use memcache::MemcacheClient;
use memcache::MemcacheSetType;

use crate::mock_store::MockStore;

#[derive(Clone)]
pub enum MemcacheHandler {
    Real(MemcacheClient),
    Mock(MockStore<Bytes>),
    Noop,
}

impl From<MemcacheClient> for MemcacheHandler {
    fn from(client: MemcacheClient) -> Self {
        MemcacheHandler::Real(client)
    }
}

impl MemcacheHandler {
    /// Returns true if this memcache handler is a no-op, and so operations
    /// can be entirely skipped.
    pub fn is_noop(&self) -> bool {
        match self {
            MemcacheHandler::Noop => true,
            MemcacheHandler::Real(_) | MemcacheHandler::Mock(_) => false,
        }
    }

    pub fn is_async(&self) -> bool {
        match self {
            MemcacheHandler::Real(_) => true,
            MemcacheHandler::Mock(_) | MemcacheHandler::Noop => false,
        }
    }

    pub async fn get(&self, key: String) -> Result<Option<Bytes>> {
        match self {
            MemcacheHandler::Real(client) => {
                client.get(key).await.map(|value| value.map(Bytes::from))
            }
            MemcacheHandler::Mock(store) => Ok(store.get(&key)),
            MemcacheHandler::Noop => Ok(None),
        }
    }

    pub async fn set<V>(&self, key: String, value: V) -> Result<()>
    where
        MemcacheSetType: From<V>,
        Bytes: From<V>,
        V: 'static,
    {
        match self {
            MemcacheHandler::Real(client) => client.set(key, value).await,
            MemcacheHandler::Mock(store) => {
                store.set(&key, value.into());
                Ok(())
            }
            MemcacheHandler::Noop => Ok(()),
        }
    }

    pub async fn set_with_ttl<V>(&self, key: String, value: V, duration: Duration) -> Result<()>
    where
        MemcacheSetType: From<V>,
        Bytes: From<V>,
        V: 'static,
    {
        match self {
            MemcacheHandler::Real(client) => client.set_with_ttl(key, value, duration).await,
            MemcacheHandler::Mock(_) => {
                // For now we ignore TTLs here
                self.set(key, value).await
            }
            MemcacheHandler::Noop => Ok(()),
        }
    }

    /// Deletes `key`.  Deleting an absent key succeeds.
    pub async fn del(&self, key: String) -> Result<()> {
        match self {
            MemcacheHandler::Real(client) => client.del(key).await,
            MemcacheHandler::Mock(store) => {
                store.del(&key);
                Ok(())
            }
            MemcacheHandler::Noop => Ok(()),
        }
    }

    pub fn create_mock() -> Self {
        MemcacheHandler::Mock(MockStore::new())
    }

    pub fn create_noop() -> Self {
        MemcacheHandler::Noop
    }

    #[cfg(test)]
    pub(crate) fn gets_count(&self) -> usize {
        use std::sync::atomic::Ordering;
        match self {
            MemcacheHandler::Real(_) | MemcacheHandler::Noop => unimplemented!(),
            MemcacheHandler::Mock(MockStore { get_count, .. }) => get_count.load(Ordering::SeqCst),
        }
    }

    pub fn mock_store(&self) -> Option<&MockStore<Bytes>> {
        match self {
            MemcacheHandler::Real(_) | MemcacheHandler::Noop => None,
            MemcacheHandler::Mock(mock) => Some(mock),
        }
    }
}
