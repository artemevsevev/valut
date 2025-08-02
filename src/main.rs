use std::{collections::HashMap, str::FromStr};

use anyhow::Result;
use chrono::NaiveDate;
use reqwest::Client;
use rust_decimal::Decimal;
use val_curs::{ValCurs, Valute};

mod val_curs;

#[tokio::main]
async fn main() -> Result<()> {
    let val_curs = get_val_curs(NaiveDate::from_ymd(2025, 8, 5)).await?;
    dbg!(&val_curs);
    let map = get_curs_map(&val_curs).await?;

    dbg!(map);

    Ok(())
}

async fn get_curs_map(val_curs: &ValCurs) -> Result<HashMap<String, Decimal>> {
    let mut map = HashMap::new();

    for valute in &val_curs.valute {
        let normalized_string = normalize_decimal_string(&valute.vunit_rate);
        let value = Decimal::from_str(&normalized_string)?;
        map.insert(valute.char_code.clone(), value);
    }

    Ok(map)
}

fn normalize_decimal_string(s: &str) -> String {
    s.replace(',', ".")
}

async fn get_val_curs(date: NaiveDate) -> Result<ValCurs> {
    let url = get_url(date).await;
    let text = load_xml(&url).await?;
    let val_curs: ValCurs = quick_xml::de::from_str(&text)?;

    Ok(val_curs)
}

async fn load_xml(url: &str) -> Result<String> {
    let client = Client::new();
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("Cannot download file: {}", resp.status());
    }

    let text = resp.text().await?;

    Ok(text)
}

async fn get_url(date: NaiveDate) -> String {
    format!(
        "https://cbr.ru/scripts/XML_daily.asp?date_req={}",
        date.format("%d/%m/%Y")
    )
}
