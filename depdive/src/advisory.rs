//! This module abstracts interaction with rustsec advisory

use anyhow::Result;
use rustsec::{
    advisory::Advisory,
    database::{Database, Query},
    package::Name,
};
use std::str::FromStr;

pub struct AdvisoryLookup {
    db: Database,
}

impl AdvisoryLookup {
    pub fn new() -> Result<Self> {
        Ok(Self {
            db: Database::fetch()?,
        })
    }

    pub fn get_crate_version_advisories(
        &self,
        name: &str,
        version: &str,
    ) -> Result<Vec<&Advisory>> {
        let query =
            Query::new().package_version(Name::from_str(name)?, rustsec::Version::parse(version)?);

        Ok(self.db.query(&query))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use once_cell::sync::Lazy;
    use rustsec::advisory::id::Id;

    static ADVISORY_LOOKUP: Lazy<AdvisoryLookup> = Lazy::new(|| AdvisoryLookup::new().unwrap());

    #[test]
    fn test_advisory_lookup() {
        let advisory = ADVISORY_LOOKUP
            .db
            .get(&Id::from_str("RUSTSEC-2016-0005").unwrap());
        assert!(advisory.is_some());
    }

    #[test]
    fn test_advisory_crate_version_lookup() {
        let advisories = ADVISORY_LOOKUP
            .get_crate_version_advisories("tokio", "1.7.1")
            .unwrap();
        assert!(!advisories.is_empty());

        let advisories = ADVISORY_LOOKUP
            .get_crate_version_advisories("::invalid::", "1.7.1")
            .unwrap();
        assert!(advisories.is_empty());
    }
}
