use std::marker::PhantomData;

use heed::types::{OwnedType, SerdeBincode};
use heed::Database;
use serde::{Deserialize, Serialize};

pub type BEu64 = heed::zerocopy::U64<heed::byteorder::BigEndian>;
pub type Key = BEu64;
pub type KeyedDb<T> = Database<OwnedType<Key>, SerdeBincode<T>>;

pub trait KeyedDbExt<T>
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    fn alloc(&self, txn: &mut heed::RwTxn, val: &T) -> heed::Result<Id<T>>;
}

impl<T> KeyedDbExt<T> for KeyedDb<T>
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    fn alloc(&self, txn: &mut heed::RwTxn, val: &T) -> heed::Result<Id<T>> {
        let idx = match self.last(txn)? {
            Some((key, _)) => key.get() + 1,
            None => 0,
        };
        let idx = Key::new(idx);
        self.put(txn, &idx, val)?;
        Ok(Id {
            idx,
            _phantom: PhantomData::default(),
        })
    }
}

#[derive(Debug)]
pub struct Id<T> {
    pub idx: Key,
    _phantom: PhantomData<T>,
}

impl<T> Serialize for Id<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.idx.get())
    }
}

impl<'de, T> Deserialize<'de> for Id<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let idx = <u64 as Deserialize<'de>>::deserialize(deserializer)?;
        Ok(Self {
            idx: Key::new(idx),
            _phantom: PhantomData::default(),
        })
    }
}

impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        Self {
            idx: self.idx,
            _phantom: PhantomData::default(),
        }
    }
}

impl<T> Copy for Id<T> {}

impl<T> AsRef<Key> for Id<T> {
    fn as_ref(&self) -> &Key {
        &self.idx
    }
}

impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx
    }
}

impl<T> Eq for Id<T> {}