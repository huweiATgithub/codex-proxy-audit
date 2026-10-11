//! Current host Key names, fetched once for a management response and never stored.
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};

use gateway_plugin_sdk::{
    call::host::{ClientKey, KeyListRequest},
    client::HostClient,
};

#[derive(Clone, Debug, Default)]
pub struct CurrentKeys(BTreeMap<String, Option<String>>);

#[derive(Debug, thiserror::Error)]
#[error("current host Key list unavailable or invalid")]
pub struct KeyLookupError;

impl CurrentKeys {
    pub fn from_keys(keys: Vec<ClientKey>) -> Result<Self, KeyLookupError> {
        let mut names = BTreeMap::new();
        for key in keys {
            if !(1..=256).contains(&key.id.chars().count()) || names.contains_key(&key.id) {
                return Err(KeyLookupError);
            }
            let name =
                (!key.name.is_empty() && key.name.chars().count() <= 512).then_some(key.name);
            names.insert(key.id, name);
        }
        Ok(Self(names))
    }

    pub async fn fetch(host: &HostClient) -> Result<Self, KeyLookupError> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut keys = Vec::new();
            let mut cursor = None;
            let mut seen = HashSet::new();
            loop {
                let page = host
                    .list_keys(KeyListRequest { cursor, limit: 100 })
                    .await
                    .map_err(|_| KeyLookupError)?;
                keys.extend(page.keys);
                if keys.len() > 100_000 {
                    return Err(KeyLookupError);
                }
                cursor = page.next_cursor;
                if cursor.is_none() {
                    return Self::from_keys(keys);
                }
                if !seen.insert(cursor.clone()) {
                    return Err(KeyLookupError);
                }
            }
        })
        .await
        .map_err(|_| KeyLookupError)?
    }

    pub fn name(&self, id: &str) -> Option<&str> {
        self.0.get(id).and_then(|name| name.as_deref())
    }

    pub(crate) fn entries(&self) -> &BTreeMap<String, Option<String>> {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, name: &str) -> ClientKey {
        ClientKey {
            id: id.into(),
            name: name.into(),
            enabled: true,
        }
    }

    #[test]
    fn complete_current_names_are_parsed_and_missing_names_stay_missing() {
        let keys = CurrentKeys::from_keys(vec![
            key("b", "新名称"),
            key("a", ""),
            key("c", &"界".repeat(513)),
        ])
        .unwrap();
        assert_eq!(keys.name("b"), Some("新名称"));
        assert_eq!(keys.name("a"), None);
        assert_eq!(keys.name("c"), None);
        assert_eq!(keys.name("deleted"), None);
        assert_eq!(
            keys.entries().keys().cloned().collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        assert!(CurrentKeys::from_keys(vec![key("", "name")]).is_err());
        assert!(CurrentKeys::from_keys(vec![key("a", "first"), key("a", "second")]).is_err());
    }
}
