use std::{
    collections::BTreeMap,
    marker::PhantomData,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
};

use alloy_rlp::{Encodable, Header};
use bytes::{Bytes, BytesMut};

use crate::{
    ENR_VERSION, Enr, EnrKey, EnrPublicKey, Error, ID_ENR_KEY, IP_ENR_KEY, IP6_ENR_KEY, Key,
    MAX_ENR_SIZE, NodeId, TCP_ENR_KEY, TCP6_ENR_KEY, UDP_ENR_KEY, UDP6_ENR_KEY,
};

/// The base builder for generating ENR records with arbitrary signing algorithms.
pub struct Builder<K: EnrKey> {
    /// The identity scheme used to build the ENR record.
    id: String,

    /// The starting sequence number for the ENR record.
    seq: u64,

    /// The key-value pairs for the ENR record.
    /// Values are stored as RLP encoded bytes.
    content: BTreeMap<Key, Bytes>,

    /// Pins the generic key types.
    phantom: PhantomData<K>,
}

impl<K: EnrKey> Default for Builder<K> {
    /// Constructs a minimal [`Builder`] for the v4 identity scheme.
    fn default() -> Self {
        Self {
            id: String::from_utf8(ENR_VERSION.into()).expect("Is a valid string"),
            seq: 1,
            content: BTreeMap::new(),
            phantom: PhantomData,
        }
    }
}

impl<K: EnrKey> Builder<K> {
    /// Modifies the sequence number of the builder.
    pub fn seq(&mut self, seq: u64) -> &mut Self {
        self.seq = seq;
        self
    }

    /// Adds an arbitrary key-value to the `ENRBuilder`.
    pub fn add_value<T: Encodable>(&mut self, key: impl AsRef<[u8]>, value: &T) -> &mut Self {
        let mut out = BytesMut::with_capacity(value.length());
        value.encode(&mut out);
        self.add_value_rlp(key, out.freeze())
    }

    /// Adds an arbitrary key-value where the value is raw RLP encoded bytes.
    pub fn add_value_rlp(&mut self, key: impl AsRef<[u8]>, rlp: Bytes) -> &mut Self {
        self.content.insert(key.as_ref().to_vec(), rlp);
        self
    }

    /// Adds an `ip`/`ip6` field to the `ENRBuilder`.
    pub fn ip(&mut self, ip: IpAddr) -> &mut Self {
        match ip {
            IpAddr::V4(addr) => self.ip4(addr),
            IpAddr::V6(addr) => self.ip6(addr),
        }
    }

    /// Adds an `ip` field to the `ENRBuilder`.
    pub fn ip4(&mut self, ip: Ipv4Addr) -> &mut Self {
        self.add_value(IP_ENR_KEY, &ip.octets().as_ref());
        self
    }

    /// Adds an `ip6` field to the `ENRBuilder`.
    pub fn ip6(&mut self, ip: Ipv6Addr) -> &mut Self {
        self.add_value(IP6_ENR_KEY, &ip.octets().as_ref());
        self
    }

    /*
     * Removed from the builder as only the v4 scheme is currently supported.
     * This is set as default in the builder.

    /// Adds an `Id` field to the `ENRBuilder`.
    pub fn id(&mut self, id: &str) -> &mut Self {
        self.add_value(ID_ENR_KEY, &id.as_bytes());
        self
    }
    */

    /// Adds a `tcp` field to the `ENRBuilder`.
    pub fn tcp4(&mut self, tcp: u16) -> &mut Self {
        self.add_value(TCP_ENR_KEY, &tcp);
        self
    }

    /// Adds a `tcp6` field to the `ENRBuilder`.
    pub fn tcp6(&mut self, tcp: u16) -> &mut Self {
        self.add_value(TCP6_ENR_KEY, &tcp);
        self
    }

    /// Adds a `udp` field to the `ENRBuilder`.
    pub fn udp4(&mut self, udp: u16) -> &mut Self {
        self.add_value(UDP_ENR_KEY, &udp);
        self
    }

    /// Adds a `udp6` field to the `ENRBuilder`.
    pub fn udp6(&mut self, udp: u16) -> &mut Self {
        self.add_value(UDP6_ENR_KEY, &udp);
        self
    }

    /// Adds a [EIP-7636](https://eips.ethereum.org/EIPS/eip-7636) `client` field to the `ENRBuilder`.
    pub fn client_info(
        &mut self,
        name: String,
        version: String,
        build: Option<String>,
    ) -> &mut Self {
        if build.is_none() {
            self.add_value("client", &vec![name, version]);
        } else {
            self.add_value("client", &vec![name, version, build.unwrap()]);
        }

        self
    }

    /// Generates the rlp-encoded form of the ENR specified by the builder config.
    fn rlp_content(&self) -> BytesMut {
        let mut list = Vec::<u8>::with_capacity(MAX_ENR_SIZE);
        self.seq.encode(&mut list);
        for (k, v) in &self.content {
            // Keys are bytes
            k.as_slice().encode(&mut list);
            // Values are raw RLP encoded data
            list.extend_from_slice(v);
        }
        let header = Header { list: true, payload_length: list.len() };
        let mut out = BytesMut::with_capacity(header.length() + list.len());
        header.encode(&mut out);
        out.extend_from_slice(&list);
        out
    }

    /// Signs record based on the identity scheme. Currently only "v4" is supported.
    fn signature(&self, key: &K) -> Result<Vec<u8>, Error> {
        match self.id.as_str() {
            "v4" => key.sign_v4(&self.rlp_content()).map_err(|_| Error::SigningError),
            // unsupported identity schemes
            _ => Err(Error::SigningError),
        }
    }

    /// Adds a public key to the ENR builder.
    fn add_public_key(&mut self, key: &K::PublicKey) {
        self.add_value(key.enr_key(), &key.encode().as_ref());
    }

    /// Constructs an ENR from the [`Builder`].
    ///
    /// # Errors
    /// Fails if the identity scheme is not supported, or the record size exceeds `MAX_ENR_SIZE`.
    pub fn build(&mut self, key: &K) -> Result<Enr<K>, Error> {
        // add the identity scheme to the content
        if self.id != "v4" {
            return Err(Error::UnsupportedIdentityScheme);
        }

        // Sanitize all data, ensuring all RLP data is correctly formatted.
        for value in self.content.values() {
            Header::decode(&mut value.as_ref())?;
        }

        let mut id_bytes = BytesMut::with_capacity(self.id.length());
        self.id.as_bytes().encode(&mut id_bytes);
        self.add_value_rlp(ID_ENR_KEY, id_bytes.freeze());

        self.add_public_key(&key.public());
        let rlp_content = self.rlp_content();

        let signature = self.signature(key)?;

        // check the size of the record
        if rlp_content.len() + signature.len() + 8 > MAX_ENR_SIZE {
            return Err(Error::ExceedsMaxSize);
        }

        Ok(Enr {
            seq: self.seq,
            node_id: NodeId::from(key.public()),
            content: self.content.clone(),
            signature,
            phantom: PhantomData,
        })
    }
}
