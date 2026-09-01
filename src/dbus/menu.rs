//! DBusMenu wire types and the conversion boundary to core-owned menu types.

use crate::core::{
    ChildrenDisplay, MenuItem, MenuItemId, MenuItemPropertiesUpdate, MenuItemType, MenuModel,
    MenuPropertyUpdate, MenuShortcut,
};
use serde::Deserialize;
use std::collections::HashMap;
use zbus::zvariant::{OwnedValue, Value};

#[derive(Debug, Deserialize, zbus::zvariant::Type)]
pub(crate) struct WireLayoutNode(
    pub(crate) i32,
    pub(crate) HashMap<String, OwnedValue>,
    pub(crate) Vec<OwnedValue>,
);

#[derive(Debug)]
pub(crate) struct LayoutNode(
    pub(crate) i32,
    pub(crate) HashMap<String, OwnedValue>,
    pub(crate) Vec<LayoutNode>,
);

pub(crate) fn convert_layout(revision: u32, layout: LayoutNode) -> Result<MenuModel, String> {
    Ok(MenuModel {
        revision,
        root: convert_item(layout)?,
    })
}

pub(crate) fn parse_layout(value: OwnedValue) -> Result<LayoutNode, String> {
    let value = match &*value {
        Value::Value(inner) => {
            OwnedValue::try_from(inner.as_ref().clone()).map_err(|error| error.to_string())?
        }
        _ => value,
    };
    let structure =
        zbus::zvariant::Structure::try_from(value).map_err(|error| error.to_string())?;
    let fields = structure.into_fields();
    if fields.len() != 3 {
        return Err("DBusMenu layout item must have three fields".into());
    }
    let id = i32::try_from(&fields[0]).map_err(|error| error.to_string())?;
    let properties_value = OwnedValue::try_from(&fields[1]).map_err(|error| error.to_string())?;
    let properties = HashMap::<String, OwnedValue>::try_from(properties_value)
        .map_err(|error| format!("invalid item properties: {error}"))?;
    let array = zbus::zvariant::Array::try_from(&fields[2]).map_err(|error| error.to_string())?;
    let children = array
        .inner()
        .iter()
        .cloned()
        .map(OwnedValue::try_from)
        .map(|value| value.map_err(|error| format!("invalid child value: {error}")))
        .map(|value| {
            value.and_then(|value| {
                parse_layout(value).map_err(|error| format!("invalid child layout: {error}"))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LayoutNode(id, properties, children))
}

pub(crate) fn parse_wire_layout(wire: WireLayoutNode) -> Result<LayoutNode, String> {
    let children = wire
        .2
        .into_iter()
        .map(|child| parse_layout(child).map_err(|error| format!("invalid child layout: {error}")))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(LayoutNode(wire.0, wire.1, children))
}

fn convert_item(raw: LayoutNode) -> Result<MenuItem, String> {
    let (id, properties, children) = (raw.0, raw.1, raw.2);
    let label = string_property(&properties, "label")?;
    let enabled = bool_property(&properties, "enabled")?.unwrap_or(true);
    let visible = bool_property(&properties, "visible")?.unwrap_or(true);
    let item_type = match string_property(&properties, "type")? {
        None => MenuItemType::Standard,
        Some(value) if value == "separator" => MenuItemType::Separator,
        Some(value) if value == "standard" || value.is_empty() => MenuItemType::Standard,
        Some(value) => MenuItemType::Unknown(value),
    };
    let children_display = match string_property(&properties, "children-display")? {
        None => None,
        Some(value) if value == "submenu" => Some(ChildrenDisplay::Submenu),
        Some(value) => Some(ChildrenDisplay::Unknown(value)),
    };
    let shortcut = properties
        .get("shortcut")
        .map(|value| {
            Vec::<Vec<String>>::try_from(value.clone())
                .map(|keys| MenuShortcut { keys })
                .map_err(|error| format!("invalid shortcut for item {id}: {error}"))
        })
        .transpose()?;
    let icon_name = string_property(&properties, "icon-name")?;

    Ok(MenuItem {
        id: MenuItemId(id),
        label,
        enabled,
        visible,
        item_type,
        children_display,
        shortcut,
        icon_name,
        action: None,
        children: children
            .into_iter()
            .map(convert_item)
            .collect::<Result<_, _>>()?,
    })
}

fn string_property(
    properties: &HashMap<String, OwnedValue>,
    name: &str,
) -> Result<Option<String>, String> {
    properties
        .get(name)
        .map(|value| {
            String::try_from(value.clone())
                .map_err(|error| format!("invalid {name} property: {error}"))
        })
        .transpose()
}

fn bool_property(
    properties: &HashMap<String, OwnedValue>,
    name: &str,
) -> Result<Option<bool>, String> {
    properties
        .get(name)
        .map(|value| {
            bool::try_from(value.clone())
                .map_err(|error| format!("invalid {name} property: {error}"))
        })
        .transpose()
}

pub(crate) fn convert_property_updates(
    updated: Vec<(i32, HashMap<String, OwnedValue>)>,
    removed: Vec<(i32, Vec<String>)>,
) -> Result<Vec<MenuItemPropertiesUpdate>, String> {
    let mut result = Vec::new();
    for (id, properties) in updated {
        let mut converted = Vec::new();
        for (name, value) in properties {
            match name.as_str() {
                "label" => converted.push(MenuPropertyUpdate::Label(Some(
                    String::try_from(value).map_err(|error| format!("invalid label: {error}"))?,
                ))),
                "enabled" => converted.push(MenuPropertyUpdate::Enabled(
                    bool::try_from(value).map_err(|error| format!("invalid enabled: {error}"))?,
                )),
                "visible" => converted.push(MenuPropertyUpdate::Visible(
                    bool::try_from(value).map_err(|error| format!("invalid visible: {error}"))?,
                )),
                "type" => {
                    let value = String::try_from(value)
                        .map_err(|error| format!("invalid type: {error}"))?;
                    converted.push(MenuPropertyUpdate::ItemType(match value.as_str() {
                        "separator" => MenuItemType::Separator,
                        "standard" | "" => MenuItemType::Standard,
                        _ => MenuItemType::Unknown(value),
                    }));
                }
                "children-display" => {
                    let value = String::try_from(value)
                        .map_err(|error| format!("invalid children-display: {error}"))?;
                    converted.push(MenuPropertyUpdate::ChildrenDisplay(Some(
                        match value.as_str() {
                            "submenu" => ChildrenDisplay::Submenu,
                            _ => ChildrenDisplay::Unknown(value),
                        },
                    )));
                }
                "shortcut" => converted.push(MenuPropertyUpdate::Shortcut(Some(
                    Vec::<Vec<String>>::try_from(value)
                        .map(|keys| MenuShortcut { keys })
                        .map_err(|error| format!("invalid shortcut: {error}"))?,
                ))),
                "icon-name" => converted.push(MenuPropertyUpdate::IconName(Some(
                    String::try_from(value)
                        .map_err(|error| format!("invalid icon-name: {error}"))?,
                ))),
                _ => {}
            }
        }
        result.push(MenuItemPropertiesUpdate {
            item_id: MenuItemId(id),
            properties: converted,
        });
    }
    for (id, properties) in removed {
        let mut converted = Vec::new();
        for name in properties {
            match name.as_str() {
                "label" => converted.push(MenuPropertyUpdate::Label(None)),
                "enabled" => converted.push(MenuPropertyUpdate::Enabled(true)),
                "visible" => converted.push(MenuPropertyUpdate::Visible(true)),
                "type" => converted.push(MenuPropertyUpdate::ItemType(MenuItemType::Standard)),
                "children-display" => converted.push(MenuPropertyUpdate::ChildrenDisplay(None)),
                "shortcut" => converted.push(MenuPropertyUpdate::Shortcut(None)),
                "icon-name" => converted.push(MenuPropertyUpdate::IconName(None)),
                _ => {}
            }
        }
        result.push(MenuItemPropertiesUpdate {
            item_id: MenuItemId(id),
            properties: converted,
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::OwnedValue;

    fn node(id: i32, props: &[(&str, OwnedValue)], children: Vec<LayoutNode>) -> LayoutNode {
        LayoutNode(
            id,
            props
                .iter()
                .map(|(k, v)| ((*k).into(), v.clone()))
                .collect(),
            children,
        )
    }
    fn s(value: &str) -> OwnedValue {
        OwnedValue::try_from(zbus::zvariant::Value::from(value.to_string())).unwrap()
    }
    fn b(value: bool) -> OwnedValue {
        value.into()
    }

    #[test]
    fn converts_nested_tree_and_defaults() {
        let root = node(
            0,
            &[],
            vec![node(
                1,
                &[("label", s("_File")), ("children-display", s("submenu"))],
                vec![node(
                    2,
                    &[
                        ("label", s("New")),
                        (
                            "shortcut",
                            zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::from(
                                vec![vec!["<Control>".to_string(), "N".to_string()]],
                            ))
                            .unwrap(),
                        ),
                    ],
                    vec![],
                )],
            )],
        );
        let model = convert_layout(1, root).unwrap();
        assert_eq!(model.revision, 1);
        assert_eq!(model.root.children[0].label.as_deref(), Some("_File"));
        assert!(model.root.enabled && model.root.visible);
        assert_eq!(
            model.root.children[0].children_display,
            Some(ChildrenDisplay::Submenu)
        );
        assert_eq!(
            model.root.children[0].children[0]
                .shortcut
                .as_ref()
                .unwrap()
                .keys[0],
            vec!["<Control>", "N"]
        );
    }

    #[test]
    fn preserves_flags_unknown_type_and_optional_properties() {
        let root = node(
            0,
            &[],
            vec![node(
                1,
                &[
                    ("label", s("Hidden")),
                    ("enabled", b(false)),
                    ("visible", b(false)),
                    ("type", s("future")),
                    ("icon-name", s("document-new")),
                ],
                vec![],
            )],
        );
        let item = &convert_layout(1, root).unwrap().root.children[0];
        assert!(!item.enabled && !item.visible);
        assert_eq!(item.item_type, MenuItemType::Unknown("future".into()));
        assert_eq!(item.icon_name.as_deref(), Some("document-new"));
    }

    #[test]
    fn property_updates_use_defaults_for_removals_and_ignore_unknown_properties() {
        let updates = convert_property_updates(
            vec![(
                1,
                [("enabled".into(), b(false)), ("future".into(), s("x"))]
                    .into_iter()
                    .collect(),
            )],
            vec![(1, vec!["enabled".into(), "visible".into(), "future".into()])],
        )
        .unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(
            updates[0].properties,
            vec![MenuPropertyUpdate::Enabled(false)]
        );
        assert_eq!(
            updates[1].properties,
            vec![
                MenuPropertyUpdate::Enabled(true),
                MenuPropertyUpdate::Visible(true)
            ]
        );
    }
}
