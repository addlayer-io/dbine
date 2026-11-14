//! Windows time-zone ids (`localTimeZoneId` of a refresh schedule) for the
//! form's select: `(id, label)`. Owned by part C; a static list.
//!
//! The ids Windows (and so Power BI) uses, from Unicode CLDR's
//! `windowsZones.xml` (each zone's territory "001" entry), in its order
//! (by UTC offset). The label puts the standard offset in front of the id;
//! ids are names, not translated.

pub const WINDOWS_TIME_ZONES: &[(&str, &str)] = &[
    (
        "Dateline Standard Time",
        "(UTC-12:00) Dateline Standard Time",
    ),
    ("UTC-11", "(UTC-11:00) UTC-11"),
    (
        "Aleutian Standard Time",
        "(UTC-10:00) Aleutian Standard Time",
    ),
    (
        "Hawaiian Standard Time",
        "(UTC-10:00) Hawaiian Standard Time",
    ),
    (
        "Marquesas Standard Time",
        "(UTC-09:30) Marquesas Standard Time",
    ),
    ("Alaskan Standard Time", "(UTC-09:00) Alaskan Standard Time"),
    ("UTC-09", "(UTC-09:00) UTC-09"),
    (
        "Pacific Standard Time (Mexico)",
        "(UTC-08:00) Pacific Standard Time (Mexico)",
    ),
    ("UTC-08", "(UTC-08:00) UTC-08"),
    ("Pacific Standard Time", "(UTC-08:00) Pacific Standard Time"),
    (
        "US Mountain Standard Time",
        "(UTC-07:00) US Mountain Standard Time",
    ),
    (
        "Mountain Standard Time (Mexico)",
        "(UTC-07:00) Mountain Standard Time (Mexico)",
    ),
    (
        "Mountain Standard Time",
        "(UTC-07:00) Mountain Standard Time",
    ),
    (
        "British Columbia Standard Time",
        "(UTC-07:00) British Columbia Standard Time",
    ),
    ("Yukon Standard Time", "(UTC-07:00) Yukon Standard Time"),
    ("Alberta Standard Time", "(UTC-07:00) Alberta Standard Time"),
    (
        "Central America Standard Time",
        "(UTC-06:00) Central America Standard Time",
    ),
    ("Central Standard Time", "(UTC-06:00) Central Standard Time"),
    (
        "Easter Island Standard Time",
        "(UTC-06:00) Easter Island Standard Time",
    ),
    (
        "Central Standard Time (Mexico)",
        "(UTC-06:00) Central Standard Time (Mexico)",
    ),
    (
        "Canada Central Standard Time",
        "(UTC-06:00) Canada Central Standard Time",
    ),
    (
        "Manitoba Standard Time",
        "(UTC-05:00) Manitoba Standard Time",
    ),
    (
        "SA Pacific Standard Time",
        "(UTC-05:00) SA Pacific Standard Time",
    ),
    (
        "Eastern Standard Time (Mexico)",
        "(UTC-05:00) Eastern Standard Time (Mexico)",
    ),
    ("Eastern Standard Time", "(UTC-05:00) Eastern Standard Time"),
    ("Haiti Standard Time", "(UTC-05:00) Haiti Standard Time"),
    ("Cuba Standard Time", "(UTC-05:00) Cuba Standard Time"),
    (
        "US Eastern Standard Time",
        "(UTC-05:00) US Eastern Standard Time",
    ),
    (
        "Turks And Caicos Standard Time",
        "(UTC-05:00) Turks And Caicos Standard Time",
    ),
    (
        "Paraguay Standard Time",
        "(UTC-04:00) Paraguay Standard Time",
    ),
    (
        "Atlantic Standard Time",
        "(UTC-04:00) Atlantic Standard Time",
    ),
    (
        "Venezuela Standard Time",
        "(UTC-04:00) Venezuela Standard Time",
    ),
    (
        "Central Brazilian Standard Time",
        "(UTC-04:00) Central Brazilian Standard Time",
    ),
    (
        "SA Western Standard Time",
        "(UTC-04:00) SA Western Standard Time",
    ),
    (
        "Pacific SA Standard Time",
        "(UTC-04:00) Pacific SA Standard Time",
    ),
    (
        "Newfoundland Standard Time",
        "(UTC-03:30) Newfoundland Standard Time",
    ),
    (
        "Tocantins Standard Time",
        "(UTC-03:00) Tocantins Standard Time",
    ),
    (
        "E. South America Standard Time",
        "(UTC-03:00) E. South America Standard Time",
    ),
    (
        "SA Eastern Standard Time",
        "(UTC-03:00) SA Eastern Standard Time",
    ),
    (
        "Argentina Standard Time",
        "(UTC-03:00) Argentina Standard Time",
    ),
    (
        "Greenland Standard Time",
        "(UTC-03:00) Greenland Standard Time",
    ),
    (
        "Montevideo Standard Time",
        "(UTC-03:00) Montevideo Standard Time",
    ),
    (
        "Magallanes Standard Time",
        "(UTC-03:00) Magallanes Standard Time",
    ),
    (
        "Saint Pierre Standard Time",
        "(UTC-03:00) Saint Pierre Standard Time",
    ),
    ("Bahia Standard Time", "(UTC-03:00) Bahia Standard Time"),
    ("UTC-02", "(UTC-02:00) UTC-02"),
    ("Azores Standard Time", "(UTC-01:00) Azores Standard Time"),
    (
        "Cape Verde Standard Time",
        "(UTC-01:00) Cape Verde Standard Time",
    ),
    ("UTC", "(UTC) UTC"),
    ("GMT Standard Time", "(UTC+00:00) GMT Standard Time"),
    (
        "Greenwich Standard Time",
        "(UTC+00:00) Greenwich Standard Time",
    ),
    (
        "Sao Tome Standard Time",
        "(UTC+00:00) Sao Tome Standard Time",
    ),
    ("Morocco Standard Time", "(UTC+01:00) Morocco Standard Time"),
    (
        "W. Europe Standard Time",
        "(UTC+01:00) W. Europe Standard Time",
    ),
    (
        "Central Europe Standard Time",
        "(UTC+01:00) Central Europe Standard Time",
    ),
    ("Romance Standard Time", "(UTC+01:00) Romance Standard Time"),
    (
        "Central European Standard Time",
        "(UTC+01:00) Central European Standard Time",
    ),
    (
        "W. Central Africa Standard Time",
        "(UTC+01:00) W. Central Africa Standard Time",
    ),
    ("Jordan Standard Time", "(UTC+02:00) Jordan Standard Time"),
    ("GTB Standard Time", "(UTC+02:00) GTB Standard Time"),
    (
        "Middle East Standard Time",
        "(UTC+02:00) Middle East Standard Time",
    ),
    ("Egypt Standard Time", "(UTC+02:00) Egypt Standard Time"),
    (
        "E. Europe Standard Time",
        "(UTC+02:00) E. Europe Standard Time",
    ),
    ("Syria Standard Time", "(UTC+02:00) Syria Standard Time"),
    (
        "West Bank Standard Time",
        "(UTC+02:00) West Bank Standard Time",
    ),
    (
        "South Africa Standard Time",
        "(UTC+02:00) South Africa Standard Time",
    ),
    ("FLE Standard Time", "(UTC+02:00) FLE Standard Time"),
    ("Israel Standard Time", "(UTC+02:00) Israel Standard Time"),
    (
        "South Sudan Standard Time",
        "(UTC+02:00) South Sudan Standard Time",
    ),
    (
        "Kaliningrad Standard Time",
        "(UTC+02:00) Kaliningrad Standard Time",
    ),
    ("Sudan Standard Time", "(UTC+02:00) Sudan Standard Time"),
    ("Libya Standard Time", "(UTC+02:00) Libya Standard Time"),
    ("Namibia Standard Time", "(UTC+02:00) Namibia Standard Time"),
    ("Arabic Standard Time", "(UTC+03:00) Arabic Standard Time"),
    ("Turkey Standard Time", "(UTC+03:00) Turkey Standard Time"),
    ("Arab Standard Time", "(UTC+03:00) Arab Standard Time"),
    ("Belarus Standard Time", "(UTC+03:00) Belarus Standard Time"),
    ("Russian Standard Time", "(UTC+03:00) Russian Standard Time"),
    (
        "E. Africa Standard Time",
        "(UTC+03:00) E. Africa Standard Time",
    ),
    ("Iran Standard Time", "(UTC+03:30) Iran Standard Time"),
    ("Arabian Standard Time", "(UTC+04:00) Arabian Standard Time"),
    (
        "Astrakhan Standard Time",
        "(UTC+04:00) Astrakhan Standard Time",
    ),
    (
        "Azerbaijan Standard Time",
        "(UTC+04:00) Azerbaijan Standard Time",
    ),
    ("Russia Time Zone 3", "(UTC+04:00) Russia Time Zone 3"),
    (
        "Mauritius Standard Time",
        "(UTC+04:00) Mauritius Standard Time",
    ),
    ("Saratov Standard Time", "(UTC+04:00) Saratov Standard Time"),
    (
        "Georgian Standard Time",
        "(UTC+04:00) Georgian Standard Time",
    ),
    (
        "Volgograd Standard Time",
        "(UTC+04:00) Volgograd Standard Time",
    ),
    (
        "Caucasus Standard Time",
        "(UTC+04:00) Caucasus Standard Time",
    ),
    (
        "Afghanistan Standard Time",
        "(UTC+04:30) Afghanistan Standard Time",
    ),
    (
        "West Asia Standard Time",
        "(UTC+05:00) West Asia Standard Time",
    ),
    (
        "Ekaterinburg Standard Time",
        "(UTC+05:00) Ekaterinburg Standard Time",
    ),
    (
        "Pakistan Standard Time",
        "(UTC+05:00) Pakistan Standard Time",
    ),
    (
        "Qyzylorda Standard Time",
        "(UTC+05:00) Qyzylorda Standard Time",
    ),
    ("India Standard Time", "(UTC+05:30) India Standard Time"),
    (
        "Sri Lanka Standard Time",
        "(UTC+05:30) Sri Lanka Standard Time",
    ),
    ("Nepal Standard Time", "(UTC+05:45) Nepal Standard Time"),
    (
        "Bangladesh Standard Time",
        "(UTC+06:00) Bangladesh Standard Time",
    ),
    ("Omsk Standard Time", "(UTC+06:00) Omsk Standard Time"),
    ("Myanmar Standard Time", "(UTC+06:30) Myanmar Standard Time"),
    ("SE Asia Standard Time", "(UTC+07:00) SE Asia Standard Time"),
    ("Altai Standard Time", "(UTC+07:00) Altai Standard Time"),
    (
        "W. Mongolia Standard Time",
        "(UTC+07:00) W. Mongolia Standard Time",
    ),
    (
        "North Asia Standard Time",
        "(UTC+07:00) North Asia Standard Time",
    ),
    (
        "N. Central Asia Standard Time",
        "(UTC+07:00) N. Central Asia Standard Time",
    ),
    ("Tomsk Standard Time", "(UTC+07:00) Tomsk Standard Time"),
    ("China Standard Time", "(UTC+08:00) China Standard Time"),
    (
        "North Asia East Standard Time",
        "(UTC+08:00) North Asia East Standard Time",
    ),
    (
        "Singapore Standard Time",
        "(UTC+08:00) Singapore Standard Time",
    ),
    (
        "W. Australia Standard Time",
        "(UTC+08:00) W. Australia Standard Time",
    ),
    ("Taipei Standard Time", "(UTC+08:00) Taipei Standard Time"),
    (
        "Ulaanbaatar Standard Time",
        "(UTC+08:00) Ulaanbaatar Standard Time",
    ),
    (
        "Aus Central W. Standard Time",
        "(UTC+08:45) Aus Central W. Standard Time",
    ),
    (
        "Transbaikal Standard Time",
        "(UTC+09:00) Transbaikal Standard Time",
    ),
    ("Tokyo Standard Time", "(UTC+09:00) Tokyo Standard Time"),
    (
        "North Korea Standard Time",
        "(UTC+09:00) North Korea Standard Time",
    ),
    ("Korea Standard Time", "(UTC+09:00) Korea Standard Time"),
    ("Yakutsk Standard Time", "(UTC+09:00) Yakutsk Standard Time"),
    (
        "Cen. Australia Standard Time",
        "(UTC+09:30) Cen. Australia Standard Time",
    ),
    (
        "AUS Central Standard Time",
        "(UTC+09:30) AUS Central Standard Time",
    ),
    (
        "E. Australia Standard Time",
        "(UTC+10:00) E. Australia Standard Time",
    ),
    (
        "AUS Eastern Standard Time",
        "(UTC+10:00) AUS Eastern Standard Time",
    ),
    (
        "West Pacific Standard Time",
        "(UTC+10:00) West Pacific Standard Time",
    ),
    (
        "Tasmania Standard Time",
        "(UTC+10:00) Tasmania Standard Time",
    ),
    (
        "Vladivostok Standard Time",
        "(UTC+10:00) Vladivostok Standard Time",
    ),
    (
        "Lord Howe Standard Time",
        "(UTC+10:30) Lord Howe Standard Time",
    ),
    (
        "Bougainville Standard Time",
        "(UTC+11:00) Bougainville Standard Time",
    ),
    ("Russia Time Zone 10", "(UTC+11:00) Russia Time Zone 10"),
    ("Magadan Standard Time", "(UTC+11:00) Magadan Standard Time"),
    ("Norfolk Standard Time", "(UTC+11:00) Norfolk Standard Time"),
    (
        "Sakhalin Standard Time",
        "(UTC+11:00) Sakhalin Standard Time",
    ),
    (
        "Central Pacific Standard Time",
        "(UTC+11:00) Central Pacific Standard Time",
    ),
    ("Russia Time Zone 11", "(UTC+12:00) Russia Time Zone 11"),
    (
        "New Zealand Standard Time",
        "(UTC+12:00) New Zealand Standard Time",
    ),
    ("UTC+12", "(UTC+12:00) UTC+12"),
    ("Fiji Standard Time", "(UTC+12:00) Fiji Standard Time"),
    (
        "Chatham Islands Standard Time",
        "(UTC+12:45) Chatham Islands Standard Time",
    ),
    ("UTC+13", "(UTC+13:00) UTC+13"),
    ("Tonga Standard Time", "(UTC+13:00) Tonga Standard Time"),
    ("Samoa Standard Time", "(UTC+13:00) Samoa Standard Time"),
    (
        "Line Islands Standard Time",
        "(UTC+14:00) Line Islands Standard Time",
    ),
];

/// Whether `id` is one of [`WINDOWS_TIME_ZONES`].
pub fn known(id: &str) -> bool {
    WINDOWS_TIME_ZONES.iter().any(|(z, _)| *z == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_has_the_common_zones_once() {
        assert!(WINDOWS_TIME_ZONES.len() > 130);
        for z in [
            "UTC",
            "Argentina Standard Time",
            "Romance Standard Time",
            "Pacific Standard Time",
        ] {
            assert!(known(z), "{z}");
        }
        let mut ids: Vec<&str> = WINDOWS_TIME_ZONES.iter().map(|(z, _)| *z).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), WINDOWS_TIME_ZONES.len());
        assert!(!known("America/Buenos_Aires"));
    }
}
