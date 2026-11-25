//! `config.client.toml` in the global tier: the apps' preferences, served by the host to every client (the Mac app,
//! iPhone, iPad). Settings are grouped (`[appearance]`), and each takes one value for every device or a table of
//! device sections (`code_font = { default = "Iosevka", ipad = "Berkeley Mono" }`); the most specific section that
//! matches a device wins. A device keeps a value of its own in its app, never in a file.

use serde::{Deserialize, Deserializer, Serialize};

/// A device a client runs on, as its view hello says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Device {
    Mac,
    Iphone,
    Ipad,
}

impl Device {
    /// The sections that apply to this device, most specific first.
    pub fn sections(self) -> &'static [DeviceSection] {
        match self {
            Device::Mac => &[DeviceSection::Mac, DeviceSection::Default],
            Device::Iphone => &[DeviceSection::Iphone, DeviceSection::Mobile, DeviceSection::Default],
            Device::Ipad => &[DeviceSection::Ipad, DeviceSection::Mobile, DeviceSection::Default],
        }
    }

    /// The sections more specific than `section` that apply to this device: the ones that would hide a value set
    /// there. Empty when `section` doesn't apply to this device.
    pub fn narrower_than(self, section: DeviceSection) -> &'static [DeviceSection] {
        let sections = self.sections();
        match sections.iter().position(|s| *s == section) {
            Some(index) => &sections[..index],
            None => &[],
        }
    }
}

/// One section of a per-device value: every device, or the devices it names. Named by device, never by OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceSection {
    /// Every device.
    Default,
    Mac,
    /// iPhone and iPad.
    Mobile,
    Iphone,
    Ipad,
}

impl DeviceSection {
    pub const ALL: [DeviceSection; 5] = [
        DeviceSection::Default,
        DeviceSection::Mac,
        DeviceSection::Mobile,
        DeviceSection::Iphone,
        DeviceSection::Ipad,
    ];
}

/// A value's device sections, as written; at least one is set.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Sections<T> {
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub default: Option<T>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub mac: Option<T>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub mobile: Option<T>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub iphone: Option<T>,
    #[serde(default = "none", skip_serializing_if = "Option::is_none")]
    pub ipad: Option<T>,
}

fn none<T>() -> Option<T> {
    None
}

impl<T> Sections<T> {
    fn empty() -> Self {
        Self {
            default: None,
            mac: None,
            mobile: None,
            iphone: None,
            ipad: None,
        }
    }

    pub fn get(&self, section: DeviceSection) -> Option<&T> {
        match section {
            DeviceSection::Default => self.default.as_ref(),
            DeviceSection::Mac => self.mac.as_ref(),
            DeviceSection::Mobile => self.mobile.as_ref(),
            DeviceSection::Iphone => self.iphone.as_ref(),
            DeviceSection::Ipad => self.ipad.as_ref(),
        }
    }

    fn slot(&mut self, section: DeviceSection) -> &mut Option<T> {
        match section {
            DeviceSection::Default => &mut self.default,
            DeviceSection::Mac => &mut self.mac,
            DeviceSection::Mobile => &mut self.mobile,
            DeviceSection::Iphone => &mut self.iphone,
            DeviceSection::Ipad => &mut self.ipad,
        }
    }

    fn is_empty(&self) -> bool {
        DeviceSection::ALL.iter().all(|section| self.get(*section).is_none())
    }

    /// Only `default` is set.
    fn only_default(&self) -> bool {
        DeviceSection::ALL
            .iter()
            .all(|section| matches!(section, DeviceSection::Default) == self.get(*section).is_some())
    }
}

/// A client setting's value: the same on every device, or per device section.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum PerDevice<T> {
    Every(T),
    Sections(Sections<T>),
}

impl<T> PerDevice<T> {
    /// The value for `device` and the section it comes from: the most specific section set.
    pub fn resolve(&self, device: Device) -> Option<(&T, DeviceSection)> {
        match self {
            PerDevice::Every(value) => Some((value, DeviceSection::Default)),
            PerDevice::Sections(sections) => device
                .sections()
                .iter()
                .find_map(|section| sections.get(*section).map(|value| (value, *section))),
        }
    }

    /// The value of one section as written (a plain value is `default`).
    pub fn get(&self, section: DeviceSection) -> Option<&T> {
        match self {
            PerDevice::Every(value) => matches!(section, DeviceSection::Default).then_some(value),
            PerDevice::Sections(sections) => sections.get(section),
        }
    }

    fn into_sections(self) -> Sections<T> {
        match self {
            PerDevice::Every(value) => Sections {
                default: Some(value),
                ..Sections::empty()
            },
            PerDevice::Sections(sections) => sections,
        }
    }

    /// The value with `section` set to `value` (removed for `None`). Setting a value also removes the sections more
    /// specific than `section` that apply to `device`, so on that device the chosen section is the one in effect;
    /// other devices' sections stay. A value left with only `default` becomes plain; one with no section is `None`.
    pub fn with_section(
        current: Option<Self>,
        section: DeviceSection,
        value: Option<T>,
        device: Device,
    ) -> Option<Self> {
        let mut sections = current.map_or_else(Sections::empty, PerDevice::into_sections);
        if value.is_some() {
            for narrower in device.narrower_than(section) {
                *sections.slot(*narrower) = None;
            }
        }
        *sections.slot(section) = value;
        if sections.is_empty() {
            None
        } else if sections.only_default() {
            sections.default.map(PerDevice::Every)
        } else {
            Some(PerDevice::Sections(sections))
        }
    }
}

impl<'de, T: serde::de::DeserializeOwned> Deserialize<'de> for PerDevice<T> {
    /// A table is the device sections (an unknown section name or an empty table is refused); anything else is the
    /// value for every device. No client setting's own value is a table.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = toml::Value::deserialize(deserializer)?;
        match raw {
            toml::Value::Table(_) => {
                let sections = Sections::<T>::deserialize(raw).map_err(D::Error::custom)?;
                match sections.is_empty() {
                    true => Err(D::Error::custom(
                        "a per-device value needs at least one of default, mac, mobile, iphone, ipad",
                    )),
                    false => Ok(PerDevice::Sections(sections)),
                }
            }
            toml::Value::String(_)
            | toml::Value::Integer(_)
            | toml::Value::Float(_)
            | toml::Value::Boolean(_)
            | toml::Value::Datetime(_)
            | toml::Value::Array(_) => T::deserialize(raw).map(PerDevice::Every).map_err(D::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFile {
    #[serde(default)]
    pub appearance: AppearanceGroup,
}

/// `[appearance]`: fonts.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppearanceGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_font: Option<PerDevice<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_font: Option<PerDevice<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<ClientFile, toml::de::Error> {
        toml::from_str(text)
    }

    fn font(text: &str) -> PerDevice<String> {
        parse(&format!("[appearance]\ncode_font = {text}\n"))
            .unwrap()
            .appearance
            .code_font
            .unwrap()
    }

    #[test]
    fn plain_values_and_sections_parse() {
        assert_eq!(font("\"Iosevka\""), PerDevice::Every("Iosevka".into()));
        let sections = font("{ default = \"Iosevka\", ipad = \"Berkeley Mono\" }");
        assert_eq!(
            sections.get(DeviceSection::Default).map(String::as_str),
            Some("Iosevka")
        );
        assert_eq!(
            sections.get(DeviceSection::Ipad).map(String::as_str),
            Some("Berkeley Mono")
        );
        assert_eq!(sections.get(DeviceSection::Mac), None);
        // Refused: an unknown section, an empty table, a wrong type in a section or as the plain value, an ungrouped
        // key, an unknown group.
        for bad in [
            "[appearance]\ncode_font = { ipda = \"x\" }\n",
            "[appearance]\ncode_font = {}\n",
            "[appearance]\ncode_font = { mac = 1 }\n",
            "[appearance]\ncode_font = 1\n",
            "code_font = \"x\"\n",
            "[looks]\ncode_font = \"x\"\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let err = parse("[appearance]\ncode_font = { ipda = \"x\" }\n").unwrap_err();
        assert!(err.to_string().contains("ipda"), "{err}");
    }

    #[test]
    fn the_most_specific_section_wins() {
        let value = font("{ default = \"D\", mobile = \"M\", ipad = \"P\" }");
        let resolved = |device| value.resolve(device).map(|(v, s)| (v.as_str(), s));
        assert_eq!(resolved(Device::Mac), Some(("D", DeviceSection::Default)));
        assert_eq!(resolved(Device::Iphone), Some(("M", DeviceSection::Mobile)));
        assert_eq!(resolved(Device::Ipad), Some(("P", DeviceSection::Ipad)));
        // No section for a device: nothing (the app's default).
        let mac_only = font("{ mac = \"X\" }");
        assert_eq!(mac_only.resolve(Device::Iphone), None);
        assert_eq!(
            mac_only.resolve(Device::Mac).map(|(v, s)| (v.as_str(), s)),
            Some(("X", DeviceSection::Mac))
        );
        assert_eq!(
            PerDevice::Every("E".to_string()).resolve(Device::Ipad),
            Some((&"E".to_string(), DeviceSection::Default))
        );
    }

    #[test]
    fn setting_a_section_hides_nothing_on_this_device() {
        let set = |current: Option<PerDevice<String>>, section, value: Option<&str>, device| {
            PerDevice::with_section(current, section, value.map(str::to_string), device)
        };
        let start = font("{ default = \"D\", mobile = \"M\", ipad = \"P\", iphone = \"I\", mac = \"C\" }");
        // An iPad choosing every iPhone and iPad drops its own `ipad`; the iPhone's stays.
        let mobile = set(Some(start.clone()), DeviceSection::Mobile, Some("N"), Device::Ipad).unwrap();
        assert_eq!(mobile.get(DeviceSection::Ipad), None);
        assert_eq!(mobile.get(DeviceSection::Iphone).map(String::as_str), Some("I"));
        assert_eq!(
            mobile.resolve(Device::Ipad).map(|(_, s)| s),
            Some(DeviceSection::Mobile)
        );
        // An iPad choosing every device drops `ipad` and `mobile`; `mac` and `iphone` stay.
        let every = set(Some(start.clone()), DeviceSection::Default, Some("E"), Device::Ipad).unwrap();
        assert_eq!(
            every.resolve(Device::Ipad).map(|(v, s)| (v.as_str(), s)),
            Some(("E", DeviceSection::Default))
        );
        assert_eq!(every.get(DeviceSection::Mobile), None);
        assert_eq!(every.get(DeviceSection::Mac).map(String::as_str), Some("C"));
        assert_eq!(every.get(DeviceSection::Iphone).map(String::as_str), Some("I"));
        // A Mac choosing every device drops `mac` only.
        let mac = set(Some(start.clone()), DeviceSection::Default, Some("E"), Device::Mac).unwrap();
        assert_eq!(mac.get(DeviceSection::Mac), None);
        assert_eq!(mac.get(DeviceSection::Ipad).map(String::as_str), Some("P"));
        // A section that doesn't apply to this device removes nothing else.
        let other = set(Some(start.clone()), DeviceSection::Mac, Some("Z"), Device::Iphone).unwrap();
        assert_eq!(other.get(DeviceSection::Iphone).map(String::as_str), Some("I"));
        // Removing one section removes only it.
        let removed = set(Some(start), DeviceSection::Ipad, None, Device::Ipad).unwrap();
        assert_eq!(removed.get(DeviceSection::Mobile).map(String::as_str), Some("M"));
        assert_eq!(removed.get(DeviceSection::Ipad), None);
    }

    #[test]
    fn values_collapse_to_plain_and_to_nothing() {
        let plain = PerDevice::with_section(None, DeviceSection::Default, Some("D".to_string()), Device::Mac);
        assert_eq!(plain, Some(PerDevice::Every("D".to_string())));
        let mac = PerDevice::with_section(None, DeviceSection::Mac, Some("C".to_string()), Device::Mac).unwrap();
        assert!(matches!(mac, PerDevice::Sections(_)));
        // Back to plain once only `default` is left; nothing once no section is.
        let both = PerDevice::with_section(Some(mac), DeviceSection::Default, Some("D".to_string()), Device::Iphone);
        let back = PerDevice::with_section(both, DeviceSection::Mac, None, Device::Mac);
        assert_eq!(back, Some(PerDevice::Every("D".to_string())));
        assert_eq!(
            PerDevice::with_section(back, DeviceSection::Default, None, Device::Mac),
            None
        );
    }

    /// Written back, a plain value is the value and sections are a table: what the file holds.
    #[test]
    fn serializes_as_written() {
        let file = parse("[appearance]\ntext_font = \"Inter\"\ncode_font = { mac = \"A\", ipad = \"B\" }\n").unwrap();
        assert_eq!(
            serde_json::to_value(&file).unwrap(),
            serde_json::json!({"appearance": {"text_font": "Inter", "code_font": {"mac": "A", "ipad": "B"}}})
        );
        // Read back from JSON, as the apps' writes are.
        let json: PerDevice<serde_json::Value> = serde_json::from_value(serde_json::json!({"ipad": "B"})).unwrap();
        assert_eq!(json.get(DeviceSection::Ipad), Some(&serde_json::json!("B")));
    }
}
