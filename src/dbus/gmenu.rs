use crate::core::{
    ChildrenDisplay, GtkMenuEndpoint, MenuAction, MenuActionTarget, MenuItem, MenuItemId,
    MenuItemType, MenuModel,
};
use std::collections::{HashMap, HashSet};
use zbus::zvariant::{OwnedValue, Value};

pub(crate) type RawItem = HashMap<String, OwnedValue>;
pub(crate) type RawMenu = (u32, u32, Vec<RawItem>);

pub(crate) fn convert_start(
    revision: u32,
    content: Vec<RawMenu>,
    actions: &HashMap<String, bool>,
) -> Result<MenuModel, String> {
    let menus: HashMap<(u32, u32), Vec<RawItem>> = content
        .into_iter()
        .map(|(group, menu, items)| ((group, menu), items))
        .collect();
    let root_key = root_menu(&menus)?;
    let mut stack = HashSet::new();
    let children = convert_menu(&menus, root_key, actions, &mut stack)?;
    Ok(MenuModel {
        revision,
        root: MenuItem {
            id: MenuItemId(0),
            label: None,
            enabled: true,
            visible: true,
            item_type: MenuItemType::Standard,
            children_display: None,
            shortcut: None,
            icon_name: None,
            action: None,
            children,
        },
    })
}

fn root_menu(menus: &HashMap<(u32, u32), Vec<RawItem>>) -> Result<(u32, u32), String> {
    let mut referenced = HashSet::new();
    for items in menus.values() {
        for item in items {
            for key in ["submenu", ":submenu", "section", ":section"] {
                if let Some(value) = item.get(key).and_then(pair) {
                    referenced.insert(value);
                }
            }
        }
    }
    menus
        .iter()
        .filter(|(key, _)| !referenced.contains(key))
        .max_by_key(|(key, items)| (items.len(), *key))
        .map(|(key, _)| *key)
        .or_else(|| menus.keys().copied().max())
        .ok_or_else(|| "GMenu Start returned no menus".into())
}

fn convert_menu(
    menus: &HashMap<(u32, u32), Vec<RawItem>>,
    key: (u32, u32),
    actions: &HashMap<String, bool>,
    stack: &mut HashSet<(u32, u32)>,
) -> Result<Vec<MenuItem>, String> {
    if !stack.insert(key) {
        return Err(format!("GMenu cycle at group {} menu {}", key.0, key.1));
    }
    let Some(items) = menus.get(&key) else {
        stack.remove(&key);
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for (index, raw) in items.iter().enumerate() {
        if let Some(section) = find_link(raw, "section") {
            if !result.is_empty() {
                result.push(separator(item_id(key, index, 0xffff_ffff)));
            }
            result.extend(convert_menu(menus, section, actions, stack)?);
            continue;
        }
        let action = raw
            .get("action")
            .or_else(|| raw.get(":action"))
            .and_then(string);
        let submenu = find_link(raw, "submenu");
        let children = submenu
            .map(|target| convert_menu(menus, target, actions, stack))
            .transpose()?
            .unwrap_or_default();
        let enabled = action
            .as_ref()
            .and_then(|name| actions.get(name))
            .copied()
            .unwrap_or(true);
        let label = raw.get("label").and_then(string);
        let target = raw.get("target").and_then(action_target);
        result.push(MenuItem {
            id: item_id(key, index, 0),
            label,
            enabled,
            visible: true,
            item_type: MenuItemType::Standard,
            children_display: submenu.map(|_| ChildrenDisplay::Submenu),
            shortcut: None,
            icon_name: None,
            action: action.map(|name| MenuAction { name, target }),
            children,
        });
    }
    stack.remove(&key);
    Ok(result)
}

fn separator(id: MenuItemId) -> MenuItem {
    MenuItem {
        id,
        label: None,
        enabled: false,
        visible: true,
        item_type: MenuItemType::Separator,
        children_display: None,
        shortcut: None,
        icon_name: None,
        action: None,
        children: Vec::new(),
    }
}

fn item_id((group, menu): (u32, u32), index: usize, salt: u32) -> MenuItemId {
    let mut hash = 2_166_136_261_u32;
    for value in [group, menu, index as u32, salt] {
        hash ^= value;
        hash = hash.wrapping_mul(16_777_619);
    }
    MenuItemId((hash & 0x7fff_ffff).max(1) as i32)
}

fn find_link(item: &RawItem, name: &str) -> Option<(u32, u32)> {
    item.get(name)
        .or_else(|| item.get(&format!(":{name}")))
        .and_then(pair)
}

fn unwrap(value: &OwnedValue) -> &Value<'_> {
    unwrap_value(value)
}

fn unwrap_value<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(inner) => unwrap_value(inner),
        value => value,
    }
}

fn pair(value: &OwnedValue) -> Option<(u32, u32)> {
    let Value::Structure(structure) = unwrap(value) else {
        return None;
    };
    let fields = structure.fields();
    if fields.len() != 2 {
        return None;
    }
    Some((
        u32::try_from(&fields[0]).ok()?,
        u32::try_from(&fields[1]).ok()?,
    ))
}

fn string(value: &OwnedValue) -> Option<String> {
    String::try_from(value.clone()).ok()
}

fn action_target(value: &OwnedValue) -> Option<MenuActionTarget> {
    match unwrap(value) {
        Value::Str(value) => Some(MenuActionTarget::String(value.to_string())),
        Value::Bool(value) => Some(MenuActionTarget::Boolean(*value)),
        Value::I32(value) => Some(MenuActionTarget::Int32(*value)),
        Value::U32(value) => Some(MenuActionTarget::Uint32(*value)),
        _ => None,
    }
}

pub(crate) fn endpoint_key(endpoint: &GtkMenuEndpoint) -> String {
    format!("{}{}", endpoint.bus_name, endpoint.menu_object_path)
}

pub(crate) fn referenced_groups(content: &[RawMenu]) -> Vec<u32> {
    let mut groups = HashSet::new();
    for (_, _, items) in content {
        for item in items {
            for name in ["submenu", ":submenu", "section", ":section"] {
                if let Some((group, _)) = item.get(name).and_then(pair) {
                    groups.insert(group);
                }
            }
        }
    }
    let mut groups: Vec<_> = groups.into_iter().filter(|group| *group != 0).collect();
    groups.sort_unstable();
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{OwnedValue, StructureBuilder, Value};

    fn value<T>(value: T) -> OwnedValue
    where
        T: Into<Value<'static>>,
    {
        OwnedValue::try_from(value.into()).expect("test value")
    }

    fn link(group: u32, menu: u32) -> OwnedValue {
        OwnedValue::try_from(
            StructureBuilder::new()
                .add_field(group)
                .add_field(menu)
                .build()
                .expect("test link"),
        )
        .expect("owned link")
    }

    fn item(properties: &[(&str, OwnedValue)]) -> RawItem {
        properties
            .iter()
            .map(|(name, value)| ((*name).into(), value.clone()))
            .collect()
    }

    #[test]
    fn converts_menu_sections_submenus_and_actions() {
        let root = vec![
            item(&[
                ("label", value("Arquivo")),
                ("action", value("app.file")),
                (":submenu", link(0, 1)),
            ]),
            item(&[("label", value("Editar"))]),
        ];
        let file = vec![
            item(&[("label", value("Novo")), ("action", value("app.new"))]),
            item(&[(":section", link(0, 2))]),
        ];
        let section = vec![item(&[("label", value("Sair"))])];
        let model = convert_start(
            7,
            vec![(0, 0, root), (0, 1, file), (0, 2, section)],
            &HashMap::from([("app.file".into(), true), ("app.new".into(), false)]),
        )
        .expect("GMenu model");

        assert_eq!(model.revision, 7);
        assert_eq!(model.root.children.len(), 2);
        assert_eq!(model.root.children[0].label.as_deref(), Some("Arquivo"));
        assert_eq!(
            model.root.children[0].children_display,
            Some(ChildrenDisplay::Submenu)
        );
        assert!(!model.root.children[0].children[0].enabled);
        assert!(matches!(
            model.root.children[0].children[1].item_type,
            MenuItemType::Separator
        ));
        assert_eq!(
            model.root.children[0].children[2].label.as_deref(),
            Some("Sair")
        );
    }

    #[test]
    fn item_ids_are_deterministic_and_targets_are_preserved() {
        let menu = vec![item(&[
            ("label", value("Abrir")),
            ("action", value("app.open")),
            ("target", value("/tmp")),
        ])];
        let actions = HashMap::from([(String::from("app.open"), true)]);
        let first = convert_start(1, vec![(0, 0, menu.clone())], &actions).unwrap();
        let second = convert_start(1, vec![(0, 0, menu)], &actions).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.root.children[0].action,
            Some(MenuAction {
                name: "app.open".into(),
                target: Some(MenuActionTarget::String("/tmp".into())),
            })
        );
    }

    #[test]
    fn missing_linked_menu_is_a_submenu_but_has_no_children() {
        let model = convert_start(
            1,
            vec![(
                0,
                0,
                vec![item(&[("label", value("Vazio")), (":submenu", link(0, 9))])],
            )],
            &HashMap::new(),
        )
        .unwrap();
        let child = &model.root.children[0];
        assert_eq!(child.children_display, Some(ChildrenDisplay::Submenu));
        assert!(child.children.is_empty());
    }

    #[test]
    fn referenced_groups_are_unique_and_exclude_root_group() {
        let content = vec![
            (
                0,
                0,
                vec![item(&[(":submenu", link(2, 0)), (":section", link(3, 0))])],
            ),
            (2, 0, vec![item(&[(":submenu", link(3, 1))])]),
        ];
        assert_eq!(referenced_groups(&content), vec![2, 3]);
    }
}
