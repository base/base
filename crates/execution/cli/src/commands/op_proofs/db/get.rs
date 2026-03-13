use base_execution_trie::MdbxProofsStorage;
use base_execution_trie::db::Tables;
use clap::Parser;
use reth_db::{
    Database, TableViewer,
    cursor::DbDupCursorRO,
    table::{DupSort, Table},
    transaction::DbTx,
};

#[derive(Debug, Parser)]
pub(super) struct GetCommand {
    table: Tables,

    key: String,

    #[arg(long)]
    subkey: Option<String>,

    #[arg(long)]
    raw: bool,
}

impl GetCommand {
    pub(super) fn execute(self, storage: &MdbxProofsStorage) -> eyre::Result<()> {
        self.table.view(&GetValueViewer {
            key: self.key,
            subkey: self.subkey,
            raw: self.raw,
            storage,
        })
    }
}

struct GetValueViewer<'a> {
    key: String,
    subkey: Option<String>,
    raw: bool,
    storage: &'a MdbxProofsStorage,
}

fn table_key<T: Table>(key: &str) -> eyre::Result<T::Key> {
    serde_json::from_str(key).map_err(|e| eyre::eyre!("failed to parse key: {e}"))
}

fn table_subkey<T: DupSort>(subkey: Option<&str>) -> eyre::Result<T::SubKey> {
    serde_json::from_str(subkey.unwrap_or_default())
        .map_err(|e| eyre::eyre!("failed to parse subkey: {e}"))
}

impl TableViewer<()> for GetValueViewer<'_> {
    type Error = eyre::Report;

    fn view<T: Table>(&self) -> eyre::Result<()> {
        self.storage.db_ref().view(|tx| {
            let key = table_key::<T>(&self.key)?;

            if self.raw {
                let result = tx.get::<reth_db::RawTable<T>>(reth_db::RawKey::new(key))?;
                match result {
                    Some(value) => {
                        println!("0x{}", hex::encode(value.raw_value()));
                    }
                    None => {
                        println!("no value found for the given key");
                    }
                }
            } else {
                let result = tx.get::<T>(key)?;
                match result {
                    Some(value) => {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    }
                    None => {
                        println!("no value found for the given key");
                    }
                }
            }

            Ok::<(), eyre::Report>(())
        })??;

        Ok(())
    }

    fn view_dupsort<T: DupSort>(&self) -> eyre::Result<()> {
        self.storage.db_ref().view(|tx| {
            let key = table_key::<T>(&self.key)?;

            if let Some(ref subkey_str) = self.subkey {
                let subkey = table_subkey::<T>(Some(subkey_str))?;

                if self.raw {
                    let mut cursor = tx.cursor_dup_read::<reth_db::RawDupSort<T>>()?;
                    let result = cursor.seek_by_key_subkey(
                        reth_db::RawKey::new(key),
                        reth_db::RawKey::new(subkey),
                    )?;
                    match result {
                        Some(value) => {
                            println!("0x{}", hex::encode(value.raw_value()));
                        }
                        None => {
                            println!("no value found for the given key/subkey");
                        }
                    }
                } else {
                    let mut cursor = tx.cursor_dup_read::<T>()?;
                    let result = cursor.seek_by_key_subkey(key, subkey)?;
                    match result {
                        Some(value) => {
                            println!("{}", serde_json::to_string_pretty(&value)?);
                        }
                        None => {
                            println!("no value found for the given key/subkey");
                        }
                    }
                }
            } else if self.raw {
                let result = tx.get::<reth_db::RawTable<T>>(reth_db::RawKey::new(key))?;
                match result {
                    Some(value) => {
                        println!("0x{}", hex::encode(value.raw_value()));
                    }
                    None => {
                        println!("no value found for the given key");
                    }
                }
            } else {
                let result = tx.get::<T>(key)?;
                match result {
                    Some(value) => {
                        println!("{}", serde_json::to_string_pretty(&value)?);
                    }
                    None => {
                        println!("no value found for the given key");
                    }
                }
            }

            Ok::<(), eyre::Report>(())
        })??;

        Ok(())
    }
}
