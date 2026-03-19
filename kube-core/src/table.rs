//! Types for the server-rendered `Table` presentation used by `kubectl get`.
use k8s_openapi::{Metadata, Resource, SubResourceScope, apimachinery::pkg::apis::meta::v1::ObjectMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    DynamicObject, PartialObjectMeta,
    metadata::TypeMeta,
    response::Status,
    watch::{Bookmark, BookmarkMeta, WatchEvent},
};

/// The definition of a single column in a [`Table`].
///
/// Mirrors `meta.k8s.io/v1` `TableColumnDefinition`.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct ColumnDefinition {
    /// Name of the column.
    pub name: String,
    /// Data type of the cells (e.g. `string`, `integer`, `date`).
    pub r#type: String,
    /// Refinement of [`type`](Self::type), such as `name`. Empty when none applies.
    pub format: String,
    /// Description of what the column contains.
    pub description: String,
    /// Ordering hint. Columns with priority `0` are always shown; higher values only in wider presentations.
    pub priority: isize,
}

/// A single row in a [`Table`], describing one resource.
///
/// Mirrors `meta.k8s.io/v1` `TableRow`.
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Row<K = DynamicObject> {
    /// Cell values for this row, positionally aligned with [`Table::column_definitions`].
    pub cells: Vec<Value>,
    /// Metadata of the resource this row represents.
    pub object: PartialObjectMeta<K>,
}

/// Metadata returned alongside a [`Table`].
///
/// A Kubernetes `Table` carries `ListMeta`, but the watcher requires resources
/// to expose an [`ObjectMeta`] via the [`Resource`](crate::Resource) trait. We
/// flatten an `ObjectMeta` (for `resourceVersion`) and additionally capture the
/// pagination fields that only exist on `ListMeta`; without them the `continue`
/// token would be dropped during deserialization, breaking paginated lists.
#[derive(Clone, Serialize, Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct TableMeta {
    /// Object-level metadata accessors, notably `resourceVersion`.
    #[serde(flatten)]
    pub object_meta: ObjectMeta,
    /// Continue token for paginated list responses, if more pages remain.
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<String>,
    /// Number of subsequent items not included in this list response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_item_count: Option<i64>,
}

/// A server-rendered tabular presentation of one or more resources.
///
/// This mirrors `meta.k8s.io/v1` `Table`, the format `kubectl get` uses for its
/// columnar output.
#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Table<K = DynamicObject> {
    /// List-level metadata for the table.
    pub metadata: TableMeta,
    /// Column headers describing each cell in [`Row::cells`].
    pub column_definitions: Option<Vec<ColumnDefinition>>,
    /// One row per resource in the response.
    pub rows: Vec<Row<K>>,
}

impl<T: Clone> Resource for Table<T> {
    type Scope = SubResourceScope;

    const API_VERSION: &'static str = "meta.k8s.io/v1";
    const GROUP: &'static str = "meta.k8s.io";
    const KIND: &'static str = "Table";
    const URL_PATH_SEGMENT: &'static str = "";
    const VERSION: &'static str = "v1";
}

impl<T: Clone> Metadata for Table<T> {
    type Ty = ObjectMeta;

    fn metadata(&self) -> &<Self as Metadata>::Ty {
        &self.metadata.object_meta
    }

    fn metadata_mut(&mut self) -> &mut <Self as Metadata>::Ty {
        &mut self.metadata.object_meta
    }
}

/// A watch event for a [`Table`] watch, decoding the payload as a full [`Table`]
/// for every variant — including `BOOKMARK`.
///
/// For `as=Table` watches the apiserver records the `k8s.io/initial-events-end`
/// annotation (which streaming lists rely on) on the bookmark's *embedded row
/// object* rather than the table's top-level metadata, so the whole table must
/// be decoded to recover it. Convert into a [`WatchEvent<Table<K>>`] via
/// [`From`]/[`Into`]; the conversion reconstructs a [`Bookmark`] carrying the
/// row object's annotations.
#[derive(Deserialize, Debug)]
#[serde(tag = "type", content = "object", rename_all = "UPPERCASE")]
pub enum TableWatchEvent<K = DynamicObject> {
    /// A table whose row(s) were added
    Added(Table<K>),
    /// A table whose row(s) were modified
    Modified(Table<K>),
    /// A table whose row(s) were deleted
    Deleted(Table<K>),
    /// A bookmark, decoded as a full table so the initial-events-end marker on
    /// the embedded row object survives
    Bookmark(Table<K>),
    /// An error
    Error(Box<Status>),
}

impl<K> From<TableWatchEvent<K>> for WatchEvent<Table<K>> {
    fn from(event: TableWatchEvent<K>) -> Self {
        match event {
            TableWatchEvent::Added(table) => WatchEvent::Added(table),
            TableWatchEvent::Modified(table) => WatchEvent::Modified(table),
            TableWatchEvent::Deleted(table) => WatchEvent::Deleted(table),
            TableWatchEvent::Bookmark(table) => WatchEvent::Bookmark(table_bookmark(table)),
            TableWatchEvent::Error(status) => WatchEvent::Error(status),
        }
    }
}

/// Reconstruct a [`Bookmark`] from a table-format bookmark event.
///
/// The resource version is taken from the table's top-level metadata (falling
/// back to the embedded row object), while the annotations — notably
/// `k8s.io/initial-events-end` — come from the embedded row object, which is
/// where the apiserver records them for `as=Table` watches.
fn table_bookmark<K>(table: Table<K>) -> Bookmark {
    let row_metadata = table.rows.into_iter().next().map(|row| row.object.metadata);
    let resource_version = table
        .metadata
        .object_meta
        .resource_version
        .or_else(|| {
            row_metadata
                .as_ref()
                .and_then(|meta| meta.resource_version.clone())
        })
        .unwrap_or_default();
    let annotations = row_metadata.and_then(|meta| meta.annotations).unwrap_or_default();
    Bookmark {
        // The watcher never inspects `types`; hardcoded to avoid a `K: Clone`
        // bound (the `Resource` consts on `Table<K>` require it).
        types: TypeMeta {
            api_version: "meta.k8s.io/v1".to_owned(),
            kind: "Table".to_owned(),
        },
        metadata: BookmarkMeta {
            resource_version,
            annotations,
        },
    }
}

#[cfg(test)]
mod test {
    use super::*;

    // A table-format `initial-events-end` bookmark places the annotation on the
    // embedded row object, not the table's top-level metadata. The conversion
    // must surface it so the watcher can detect the end of the initial list.
    #[test]
    fn table_bookmark_recovers_initial_events_end_annotation() {
        let line = r#"{
            "type": "BOOKMARK",
            "object": {
                "kind": "Table",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": { "resourceVersion": "12345" },
                "columnDefinitions": null,
                "rows": [{
                    "cells": [],
                    "object": {
                        "kind": "PartialObjectMetadata",
                        "apiVersion": "meta.k8s.io/v1",
                        "metadata": {
                            "resourceVersion": "12345",
                            "annotations": { "k8s.io/initial-events-end": "true" }
                        }
                    }
                }]
            }
        }"#;

        let raw: TableWatchEvent = serde_json::from_str(line).unwrap();
        match WatchEvent::from(raw) {
            WatchEvent::Bookmark(bookmark) => {
                assert_eq!(bookmark.metadata.resource_version, "12345");
                assert_eq!(
                    bookmark.metadata.annotations.get("k8s.io/initial-events-end"),
                    Some(&"true".to_owned())
                );
            }
            other => panic!("expected bookmark, got {other:?}"),
        }
    }

    #[test]
    fn table_watch_event_added_preserves_rows() {
        let line = r#"{
            "type": "ADDED",
            "object": {
                "kind": "Table",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": { "resourceVersion": "7" },
                "columnDefinitions": [
                    { "name": "Name", "type": "string", "format": "name", "description": "", "priority": 0 }
                ],
                "rows": [{
                    "cells": ["nginx"],
                    "object": {
                        "kind": "PartialObjectMetadata",
                        "metadata": { "name": "nginx", "resourceVersion": "7" }
                    }
                }]
            }
        }"#;

        let raw: TableWatchEvent = serde_json::from_str(line).unwrap();
        match WatchEvent::from(raw) {
            WatchEvent::Added(table) => {
                assert_eq!(table.metadata.object_meta.resource_version.as_deref(), Some("7"));
                assert_eq!(table.rows.len(), 1);
                assert_eq!(table.rows[0].cells, vec![Value::from("nginx")]);
            }
            other => panic!("expected added, got {other:?}"),
        }
    }
}
