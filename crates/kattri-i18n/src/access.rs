//! Accessible names for interactive controls (M4-19).
//!
//! [`CONTROLS`] maps each stable control id to the catalog message that names the control for
//! assistive technology. The egui client reads the table through [`accessible_names`] and applies
//! each name to its widget. The egui wiring lives in the UI crate. This module only holds the
//! table and a pure function over a [`Catalog`], so it has no UI dependency.

use crate::{catalog::Catalog, ids};

/// One interactive control and the message that names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlName {
    /// Stable id of the control, `area.name` in lower case.
    pub control_id: &'static str,
    /// Message id in the catalog that holds the accessible name.
    pub message_id: &'static str,
}

/// Every interactive control on the pilot screens, with its accessible-name message.
pub const CONTROLS: &[ControlName] = &[
    ControlName {
        control_id: "connect.server_address",
        message_id: ids::ACCESS_CONNECT_SERVER_ADDRESS,
    },
    ControlName {
        control_id: "connect.username",
        message_id: ids::ACCESS_CONNECT_USERNAME,
    },
    ControlName {
        control_id: "connect.password",
        message_id: ids::ACCESS_CONNECT_PASSWORD,
    },
    ControlName {
        control_id: "connect.allow_public_http",
        message_id: ids::ACCESS_CONNECT_ALLOW_PUBLIC_HTTP,
    },
    ControlName {
        control_id: "connect.submit",
        message_id: ids::ACCESS_CONNECT_SUBMIT,
    },
    ControlName {
        control_id: "browse.search_field",
        message_id: ids::ACCESS_BROWSE_SEARCH_FIELD,
    },
    ControlName {
        control_id: "browse.search_clear",
        message_id: ids::ACCESS_BROWSE_SEARCH_CLEAR,
    },
    ControlName {
        control_id: "browse.sort",
        message_id: ids::ACCESS_BROWSE_SORT,
    },
    ControlName {
        control_id: "browse.clear_filters",
        message_id: ids::ACCESS_BROWSE_CLEAR_FILTERS,
    },
    ControlName {
        control_id: "browse.next_page",
        message_id: ids::ACCESS_BROWSE_NEXT_PAGE,
    },
    ControlName {
        control_id: "browse.retry",
        message_id: ids::ACCESS_BROWSE_RETRY,
    },
    ControlName {
        control_id: "nav.high_contrast",
        message_id: ids::ACCESS_NAV_HIGH_CONTRAST,
    },
    ControlName {
        control_id: "nav.disconnect",
        message_id: ids::ACCESS_NAV_DISCONNECT,
    },
];

/// The accessible name of one control, as resolved from a catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessibleName {
    pub control_id: &'static str,
    /// The name text. Empty when the catalog has no message for the control.
    pub name: String,
}

/// The accessible name of every control in [`CONTROLS`], in table order.
///
/// A control whose message is missing from `catalog` gets an empty name. The tests reject that
/// case for the shipped en-US catalog.
pub fn accessible_names(catalog: &Catalog) -> Vec<AccessibleName> {
    CONTROLS
        .iter()
        .map(|control| AccessibleName {
            control_id: control.control_id,
            name: catalog.get(control.message_id).unwrap_or("").to_owned(),
        })
        .collect()
}
