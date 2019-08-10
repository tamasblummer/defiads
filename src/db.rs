//
// Copyright 2019 Tamas Blummer
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use bitcoin_hashes::{
    sha256d, sha256,
    hex::{FromHex, ToHex}
};
use log::Level;
use rusqlite::{Connection, Transaction, ToSql};
use std::net::SocketAddr;
use crate::error::BiadNetError;
use std::time::SystemTime;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::collections::HashSet;
use rand::{thread_rng, Rng};
use crate::content::Content;
use serde_cbor;
use rusqlite::NO_PARAMS;
use crate::iblt::IBLT;
use crate::content::ContentKey;
use crate::iblt::min_sketch;
use rand_distr::Poisson;

pub type SharedDB = Arc<Mutex<DB>>;

// number of hash functions
const NH:usize = 4;
// random, I swear
const K0:u64 = 1614418600579272000;
const K1:u64 = 8727507265883984962;


pub struct DB {
    connection: Connection
}

impl DB {
    pub fn memory () -> Result<DB, BiadNetError> {
        Ok(DB{connection: Connection::open_in_memory()?})
    }

    pub fn new(path: &std::path::Path) -> Result<DB, BiadNetError> {
        Ok(DB{connection: Connection::open(path)?})
    }

    pub fn transaction(&mut self) -> TX {
        TX{tx: self.connection.transaction().expect("can not start db transaction")}
    }
}

pub struct TX<'db> {
    tx: Transaction<'db>
}

impl<'db> TX<'db> {
    pub fn commit (self) {
        self.tx.commit().expect("failed to commit db transaction");
    }

    pub fn rollback(self) {
        self.tx.rollback().expect("failed to roll back db transaction");
    }

    pub fn create_tables (&mut self) {
        self.tx.execute_batch(r#"
            create table if not exists address (
                network text,
                ip text,
                last_seen number,
                banned number,
                primary key(network, ip)
            ) without rowid;

            create table if not exists content (
                id text primary key,
                cat text,
                abs text,
                ad blob,
                block_id text,
                height number,
                proof blob,
                publisher blob,
                term number,
                weight number,
                length number
            ) without rowid;

        "#).expect("failed to create db tables");
    }

    pub fn compute_iblt (&mut self, len: u32) -> Result<IBLT<ContentKey>, BiadNetError> {
        let mut iblt = IBLT::new(len, NH, K0, K1);

        let mut query = self.tx.prepare(r#"
            select id from content
        "#)?;
        for r in query.query_map::<String,&[&ToSql],_>(NO_PARAMS,
                                                              |r| Ok(r.get(0)?))? {
            if let Ok(id) = r {
                iblt.insert(&ContentKey::new(&sha256::Hash::from_hex(id.as_str())?[..]));
            }
        }
        Ok(iblt)
    }

    pub fn compute_min_sketch (&mut self, len: usize) -> Result<(Vec<u64>, Vec<(u64, u64)>, u32), BiadNetError> {
        let mut query = self.tx.prepare(r#"
            select id from content
        "#)?;
        let mut key_iterator = query.query_map::<String,&[&ToSql],_>(NO_PARAMS,
                                                     |r| Ok(r.get(0)?))?
            .filter_map(|r| if let Ok(id) = r {
                Some(ContentKey::new(&sha256::Hash::from_hex(id.as_str()).unwrap()[..]))}else{None});

        Ok(min_sketch(len, K0, K1, &mut key_iterator))
    }

    pub fn store_content(&mut self, height: u32, block_id: &sha256d::Hash, c: &Content, amount: u64) -> Result<usize, BiadNetError> {
        let id = c.ad.digest();
        let ser_ad = c.ad.serialize();
        let proof = serde_cbor::ser::to_vec_packed(&c.funding).unwrap();
        let publisher = serde_cbor::ser::to_vec_packed(&c.funder).unwrap();
        let length = c.length();
        debug!("store content {}", id);
        Ok(self.tx.execute(r#"
            insert or replace into content (id, cat, abs, ad, block_id, height, proof, publisher, term, weight, length)
            values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
        "#, &[&id.to_hex() as &ToSql,
            &c.ad.cat, &c.ad.abs, &ser_ad,
            &block_id.to_hex(), &height, &proof, &publisher, &c.term,
            &((amount / length as u64) as u32), &length]
        )?)
    }

    pub fn read_content(&self, digest: &sha256::Hash) -> Result<Option<Content>, BiadNetError> {
        Ok(self.tx.query_row(r#"
            select (ad, proof, publisher, term)
            from content where id = ?1
        "#, &[digest.to_hex()], |r| Ok(
            Some(Content {
                ad: serde_cbor::from_reader(std::io::Cursor::new(r.get_unwrap::<usize, Vec<u8>>(0))).unwrap(),
                funding: serde_cbor::from_reader(std::io::Cursor::new(r.get_unwrap::<usize, Vec<u8>>(1))).unwrap(),
                funder: serde_cbor::from_reader(std::io::Cursor::new(r.get_unwrap::<usize, Vec<u8>>(2))).unwrap(),
                term: r.get_unwrap(3)
            })
        ))?)
    }

    pub fn truncate_content(&mut self, limit: u64) -> Result<Vec<ContentKey>, BiadNetError> {
        let mut statement = self.tx.prepare(r#"
            select id, length from content order by weight desc
        "#)?;

        let mut to_delete = Vec::new();
        let mut size = 0u64;
        for result in statement.query_map(NO_PARAMS,
                                                |r|
                                                    Ok((r.get_unwrap::<usize, String>(0),
                                                        r.get_unwrap::<usize, i64>(1))))? {
            if let Ok((id, length)) = result {
                size += length as u64;
                if size > limit {
                    to_delete.push(id);
                }
            }
        }
        let mut keys = Vec::new();
        for id in &to_delete {
            keys.push(ContentKey::new(&sha256::Hash::from_hex(id.as_str())?[..]));
            debug!("drop content due to strorage limit {}", id);
            self.tx.execute(r#"
                delete from content where id = ?1
                            "#, &[id as &ToSql])?;
        }
        Ok(keys)
    }

    pub fn delete_expired(&mut self, height: u32) -> Result<Vec<ContentKey>, BiadNetError> {
        let mut keys = Vec::new();
        self.tx.execute(r#"
            create temp table ids (
                id text
            );
        "#, NO_PARAMS)?;
        self.tx.execute(r#"
            insert into temp.ids (id) select id from content where height + term <= ?1
        "#, &[&height as &ToSql])?;

        let mut query = self.tx.prepare(r#"
            select id from temp.ids
        "#)?;

        for r in query.query_map::<String,&[&ToSql],_>(NO_PARAMS,
                                                               |r| Ok(r.get(0)?))? {
            if let Ok(id) = r {
                keys.push(ContentKey::new(&sha256::Hash::from_hex(id.as_str())?[..]));
            }
        }

        if log_enabled!(Level::Debug) {
            let mut statement = self.tx.prepare(r#"
                select id from temp.ids
            "#)?;
            for id in statement
                .query_map(NO_PARAMS, |r| Ok(r.get_unwrap::<usize, String>(0)))? {
                if let Ok(s) = id {
                    debug!("drop expired content {}", s);
                }
            }
        }

        self.tx.execute_batch(r#"
            delete from content where id in (select id from temp.ids);
            drop table temp.ids;
        "#)?;

        Ok(keys)
    }

    pub fn delete_confirmed(&mut self, block_id: &sha256d::Hash) -> Result<Vec<ContentKey>, BiadNetError> {
        let mut keys = Vec::new();
        self.tx.execute(r#"
            create temp table ids (
                id text
            );
        "#, NO_PARAMS)?;
        self.tx.execute(r#"
            insert into temp.ids (id) select id from content where block_id = ?1
        "#, &[&block_id.to_hex() as &ToSql])?;

        let mut query = self.tx.prepare(r#"
            select id from temp.ids
        "#)?;

        for r in query.query_map::<String,&[&ToSql],_>(NO_PARAMS,
                                                              |r| Ok(r.get(0)?))? {
            if let Ok(id) = r {
                keys.push(ContentKey::new(&sha256::Hash::from_hex(id.as_str())?[..]));
            }
        }

        if log_enabled!(Level::Debug) {
            let mut statement = self.tx.prepare(r#"
                select ip from temp.ids
            "#)?;
            for id in statement
                .query_map(NO_PARAMS, |r| Ok(r.get_unwrap::<usize, String>(0)))? {
                if let Ok(s) = id {
                    debug!("drop content due to chain re-org {}", s);
                }
            }
        }

        self.tx.execute_batch(r#"
            delete from content where id in (select id from temp.ids);
            drop table temp.ids;
        "#)?;

        Ok(keys)
    }

    pub fn store_address(&mut self, network: &str, address: &SocketAddr, mut last_seen: u64, mut banned: u64) -> Result<usize, BiadNetError>  {
        if let Ok((ls, b)) = self.tx.query_row(r#"
                select last_seen, banned from address where network = ?1 and ip = ?2
            "#, &[&network.to_string() as &dyn ToSql, &address.to_string()],
                                               |r| Ok((r.get(0).unwrap_or(0i64),
                                                       r.get(1).unwrap_or(0i64)))) {
            // do not reduce last_seen or banned fields
            last_seen = std::cmp::max(ls as u64, last_seen);
            banned = std::cmp::max(b as u64, banned);
        }
        Ok(
            self.tx.execute(r#"
                insert or replace into address (network, ip, last_seen, banned) values (?1, ?2, ?3, ?4)
            "#, &[&network.to_string() as &dyn ToSql, &address.to_string(), &(last_seen as i64), &(banned as i64)])?
        )
    }

    // get an address not banned during the last day
    // the probability to be selected is exponentially higher for those with higher last_seen time
    pub fn get_an_address(&self, network: &str, other_than: &HashSet<SocketAddr>) -> Result<Option<SocketAddr>, BiadNetError> {
        const BAN_TIME: u64 = 60*60*24; // a day

        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let mut statement = self.tx.prepare(r#"
            select ip from address where network = ?1 and banned < ?2 order by last_seen desc
        "#)?;
        let eligible = statement.query_map::<SocketAddr, _, _>(
            &[&((now - BAN_TIME) as i64) as &ToSql, &network.to_string()],
            |row| {
                let s = row.get_unwrap::<usize, String>(0);
                let addr = SocketAddr::from_str(s.as_str()).expect("address stored in db should be parsable");
                Ok(addr) })?
            .filter_map(|socket|
                match socket {
                    Ok(a) => if !other_than.contains(&a) { Some(a)} else {None},
                    Err(_) =>   None }).collect::<Vec<_>>();
        let len = eligible.len();
        if len == 0 {
            return Ok(None);
        }
        Ok(Some(
            eligible[
                std::cmp::min(len-1, thread_rng().sample::<f64,_>(Poisson::new(len as f64 / 4.0).unwrap()) as usize)]))
    }

    pub fn list_abstracts(&mut self, cats: Vec<String>) -> Result<Vec<(String, String)>, BiadNetError> {
        // mut &self because using temp table
        self.tx.execute(r#"
            create temp table cats (
                cat text
            );
        "#, NO_PARAMS)?;
        for c in &cats {
            self.tx.execute(r#"
                insert into temp.cats (cat) values(?1)
            "#, &[c as &ToSql])?;
        }
        let mut statement = self.tx.prepare(r#"
            select id, abs from content where cat in (select cat from temp.cats)
        "#)?;

        let result = statement.query_map(NO_PARAMS, |r| {
            Ok((r.get_unwrap::<usize, String>(0), r.get_unwrap::<usize, String>(1)))
        })?.filter_map(|r| if let Ok(pair) = r { Some(pair) } else {None})
            .collect::<Vec<_>>();

        self.tx.execute(r#"
            drop table temp.cats
        "#, NO_PARAMS)?;

        Ok(result)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::str::FromStr;
    use bitcoin::BitcoinHash;
    use bitcoin_wallet::proved::ProvedTransaction;
    use bitcoin::blockdata::constants::genesis_block;
    use bitcoin::network::constants::Network;
    use bitcoin::util::key::PublicKey;
    use crate::ad::Ad;
    use std::time::UNIX_EPOCH;

    #[test]
    pub fn test_db () {
        let mut db = DB::memory().unwrap();
        {
            let addr = SocketAddr::from_str("127.0.0.1:8444").unwrap();
            let mut seen = HashSet::new();
            seen.insert (addr);

            let mut tx = db.transaction();
            tx.create_tables();
            // store address
            tx.store_address("biadnet", &addr, 0, 0).unwrap();
            // update
            tx.store_address("biadnet", &addr, 1, 1).unwrap();
            //find
            tx.get_an_address("biadnet", &HashSet::new()).unwrap();
            // should not find if seen
            assert!(tx.get_an_address("biadnet", &seen).unwrap().is_none());
            // ban
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            tx.store_address("biadnet", &SocketAddr::from_str("127.0.0.1:8444").unwrap(), 1, now).unwrap();
            // should not find if banned
            assert!(tx.get_an_address("biadnet", &HashSet::new()).unwrap().is_none());
            tx.commit();
        }
        {
            let mut tx = db.transaction();
            let block = genesis_block(Network::Bitcoin);
            let satoshi_key = PublicKey::from_slice(&block.txdata[0].output[0].script_pubkey [1..66]).unwrap();
            let ad = Ad::new("a".to_string(), "b".to_string(), "c");
            let content = Content{
                ad: ad.clone(),
                funding: ProvedTransaction::new(&block, 0),
                funder: satoshi_key,
                term: 1
            };
            tx.store_content(0, &block.bitcoin_hash(), &content, 5000000000).unwrap();
            tx.delete_confirmed(&block.bitcoin_hash()).unwrap();
            tx.delete_expired(1).unwrap();
            tx.truncate_content(1024).unwrap();
            tx.commit();
        }
    }
}