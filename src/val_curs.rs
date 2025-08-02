use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct Valute {
    #[serde(rename = "CharCode")]
    pub char_code: String,
    #[serde(rename = "VunitRate")]
    pub vunit_rate: String,
}

#[derive(Debug, Deserialize, Serialize, PartialEq)]
pub struct ValCurs {
    #[serde(rename = "Valute")]
    pub valute: Vec<Valute>,
}
