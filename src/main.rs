use std::{collections::HashMap, str::FromStr};

use anyhow::Result;
use chrono::{Days, NaiveDate, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use val_curs::ValCurs;

mod val_curs;

#[tokio::main]
async fn main() -> Result<()> {
    let today = Utc::now().date_naive();
    let start_date = today
        .checked_sub_days(Days::new(6))
        .ok_or(anyhow::anyhow!("Can't get previous date for {}", today))?;
    let end_date = today
        .checked_add_days(Days::new(1))
        .ok_or(anyhow::anyhow!("Can't get next date for {}", today))?;

    iterate(start_date, end_date).await?;

    Ok(())
}

async fn iterate(start_date: NaiveDate, end_date: NaiveDate) -> Result<()> {
    if start_date > end_date {
        return Err(anyhow::anyhow!("Start date must be before end date"));
    }

    let mut current_date = end_date;

    while current_date >= start_date {
        let exchange_rates = get_exchange_rates_for_date(current_date).await?;
        println!("Exchange rates for {}: {:?}", current_date, exchange_rates);

        current_date = current_date
            .pred_opt()
            .ok_or(anyhow::anyhow!("Can't get pred date for {}", current_date))?;
    }

    Ok(())
}

async fn get_exchange_rates_for_date(date: NaiveDate) -> Result<HashMap<String, Decimal>> {
    let val_curs = get_val_curs(date).await?;
    Ok(get_curs_map(&val_curs).await?)
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
        anyhow::bail!("Can't download the file: {}", resp.status());
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
