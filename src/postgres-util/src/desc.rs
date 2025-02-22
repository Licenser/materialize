// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Descriptions of PostgreSQL objects.

use std::collections::BTreeSet;

use anyhow::bail;
use proptest::prelude::{any, Arbitrary};
use proptest::strategy::{BoxedStrategy, Strategy};
use serde::{Deserialize, Serialize};
use tokio_postgres::types::Oid;
use tracing::warn;

use mz_proto::{RustType, TryFromProtoError};

include!(concat!(env!("OUT_DIR"), "/mz_postgres_util.desc.rs"));

/// Describes a schema in a PostgreSQL database.
///
/// <https://www.postgresql.org/docs/current/catalog-pg-namespace.html>
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PostgresSchemaDesc {
    /// The OID of the schema.
    pub oid: Oid,
    /// The name of the schema.
    pub name: String,
    /// Owner of the namespace
    pub owner: Oid,
}

/// Describes a table in a PostgreSQL database.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PostgresTableDesc {
    /// The OID of the table.
    pub oid: Oid,
    /// The name of the schema that the table belongs to.
    pub namespace: String,
    /// The name of the table.
    pub name: String,
    /// The description of each column, in order of their position in the table.
    pub columns: Vec<PostgresColumnDesc>,
    /// Applicable keys for this table (i.e. primary key and unique
    /// constraints).
    pub keys: BTreeSet<PostgresKeyDesc>,
}

impl PostgresTableDesc {
    /// Determines if two `PostgresTableDesc` are compatible with one another in
    /// a way that Materialize can handle.
    ///
    /// Currently this means that the values are equal except for the following
    /// exceptions:
    /// - `self`'s columns are a compatible prefix of `other`'s columns.
    ///   Compatibility is defined as returning `true` for
    ///   `PostgresColumnDesc::is_compatible`.
    /// - `self`'s keys are all present in `other`
    pub fn determine_compatibility(&self, other: &PostgresTableDesc) -> Result<(), anyhow::Error> {
        if self == other {
            return Ok(());
        }

        let PostgresTableDesc {
            oid: other_oid,
            namespace: other_namespace,
            name: other_name,
            columns: other_cols,
            keys: other_keys,
        } = other;

        // Table columns cannot change position, so only need to ensure that
        // `self.columns` is a prefix of `other_cols`.
        if self.columns.len() <= other_cols.len()
            && self.columns.iter().zip(other_cols.iter()).all(|(s, o)| s.is_compatible(o))
            && &self.name == other_name
            && &self.oid == other_oid
            && &self.namespace == other_namespace
            // Our keys are all still present in exactly the same shape.
            && self.keys.difference(other_keys).next().is_none()
        {
            Ok(())
        } else {
            warn!(
                "Error validating table in publication. Expected: {:?} Actual: {:?}",
                &self, other
            );
            bail!(
                "source table {} with oid {} has been altered",
                self.name,
                self.oid
            )
        }
    }
}

impl RustType<ProtoPostgresTableDesc> for PostgresTableDesc {
    fn into_proto(&self) -> ProtoPostgresTableDesc {
        ProtoPostgresTableDesc {
            oid: self.oid,
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            columns: self.columns.iter().map(|c| c.into_proto()).collect(),
            keys: self.keys.iter().map(PostgresKeyDesc::into_proto).collect(),
        }
    }

    fn from_proto(proto: ProtoPostgresTableDesc) -> Result<Self, TryFromProtoError> {
        Ok(PostgresTableDesc {
            oid: proto.oid,
            namespace: proto.namespace.clone(),
            name: proto.name.clone(),
            columns: proto
                .columns
                .into_iter()
                .map(PostgresColumnDesc::from_proto)
                .collect::<Result<_, _>>()?,
            keys: proto
                .keys
                .into_iter()
                .map(PostgresKeyDesc::from_proto)
                .collect::<Result<_, _>>()?,
        })
    }
}

impl Arbitrary for PostgresTableDesc {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            any::<String>(),
            any::<String>(),
            any::<u32>(),
            any::<Vec<PostgresColumnDesc>>(),
            any::<BTreeSet<PostgresKeyDesc>>(),
        )
            .prop_map(|(name, namespace, oid, columns, keys)| PostgresTableDesc {
                name,
                namespace,
                oid,
                columns,
                keys,
            })
            .boxed()
    }
}

/// Describes a column in a [`PostgresTableDesc`].
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct PostgresColumnDesc {
    /// The name of the column.
    pub name: String,
    /// The column's monotonic position in its table, i.e. "this was the _i_th
    /// column created" irrespective of the current number of columns.
    // TODO(migration): remove option in version v.51 (released in v0.49 + 1
    // additional release)
    pub col_num: Option<u16>,
    /// The OID of the column's type.
    pub type_oid: Oid,
    /// The modifier for the column's type.
    pub type_mod: i32,
    /// True if the column lacks a `NOT NULL` constraint.
    pub nullable: bool,
}

impl PostgresColumnDesc {
    /// Determines if data a relation with a structure of `other` can be treated
    /// the same as `self`.
    ///
    /// Note that this function somewhat unnecessarily errors if the names
    /// differ; this is negotiable but we want users to understand the fixedness
    /// of names in our schemas.
    fn is_compatible(&self, other: &PostgresColumnDesc) -> bool {
        self.name == other.name
            && self.col_num == other.col_num
            && self.type_oid == other.type_oid
            && self.type_mod == other.type_mod
            // Columns are compatible if:
            // - self is nullable; introducing a not null constraint doesn't
            //   change this column's behavior.
            // - self and other are both not nullable
            && (self.nullable || self.nullable == other.nullable)
    }
}

impl RustType<ProtoPostgresColumnDesc> for PostgresColumnDesc {
    fn into_proto(&self) -> ProtoPostgresColumnDesc {
        ProtoPostgresColumnDesc {
            name: self.name.clone(),
            col_num: self.col_num.map(|c| c.into()),
            type_oid: self.type_oid,
            type_mod: self.type_mod,
            nullable: self.nullable,
        }
    }

    fn from_proto(proto: ProtoPostgresColumnDesc) -> Result<Self, TryFromProtoError> {
        Ok(PostgresColumnDesc {
            name: proto.name,
            col_num: proto
                .col_num
                .map(|c| c.try_into().expect("values roundtrip")),
            type_oid: proto.type_oid,
            type_mod: proto.type_mod,
            nullable: proto.nullable,
        })
    }
}

impl Arbitrary for PostgresColumnDesc {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            any::<String>(),
            any::<u16>(),
            any::<u32>(),
            any::<i32>(),
            any::<bool>(),
        )
            .prop_map(
                |(name, col_num, type_oid, type_mod, nullable)| PostgresColumnDesc {
                    name,
                    col_num: Some(col_num),
                    type_oid,
                    type_mod,
                    nullable,
                },
            )
            .boxed()
    }
}

/// Describes a key in a [`PostgresTableDesc`].
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, PartialOrd, Ord)]
pub struct PostgresKeyDesc {
    /// This key is derived from the `pg_constraint` with this OID.
    pub oid: Oid,
    /// The name of the constraints.
    pub name: String,
    /// The `attnum` of the columns comprising the key. `attnum` is a unique identifier for a column
    /// in a PG table; see <https://www.postgresql.org/docs/current/catalog-pg-attribute.html>
    pub cols: Vec<u16>,
    /// Whether or not this key is the primary key.
    pub is_primary: bool,
    /// If this constraint was generated with NULLS NOT DISTINCT; see
    /// <https://www.postgresql.org/about/featurematrix/detail/392/>
    pub nulls_not_distinct: bool,
}

impl RustType<ProtoPostgresKeyDesc> for PostgresKeyDesc {
    fn into_proto(&self) -> ProtoPostgresKeyDesc {
        ProtoPostgresKeyDesc {
            oid: self.oid,
            name: self.name.clone(),
            cols: self.cols.clone().into_iter().map(u32::from).collect(),
            is_primary: self.is_primary,
            nulls_not_distinct: self.nulls_not_distinct,
        }
    }

    fn from_proto(proto: ProtoPostgresKeyDesc) -> Result<Self, TryFromProtoError> {
        Ok(PostgresKeyDesc {
            oid: proto.oid,
            name: proto.name,
            cols: proto
                .cols
                .into_iter()
                .map(|c| c.try_into().expect("values roundtrip"))
                .collect(),
            is_primary: proto.is_primary,
            nulls_not_distinct: proto.nulls_not_distinct,
        })
    }
}

impl Arbitrary for PostgresKeyDesc {
    type Strategy = BoxedStrategy<Self>;
    type Parameters = ();

    fn arbitrary_with(_: Self::Parameters) -> Self::Strategy {
        (
            any::<u32>(),
            any::<String>(),
            any::<Vec<u16>>(),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(
                |(oid, name, cols, is_primary, nulls_not_distinct)| PostgresKeyDesc {
                    oid,
                    name,
                    cols,
                    is_primary,
                    nulls_not_distinct,
                },
            )
            .boxed()
    }
}
