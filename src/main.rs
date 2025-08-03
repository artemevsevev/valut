use std::{collections::HashMap, env, str::FromStr};

use anyhow::{Result, anyhow};
use chrono::{Days, NaiveDate, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use sqlx::{PgPool, Pool, Postgres};
use val_curs::ValCurs;

use crate::exchange_rate::ExchangeRate;

mod exchange_rate;
mod val_curs;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

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
    let pool = get_db_pool().await?;
    let currencies = get_currencies();

    while current_date >= start_date {
        let exchange_rates = get_exchange_rates_for_date(current_date).await?;

        update_stored_exchange_rates(&current_date, &exchange_rates, &pool, &currencies).await?;

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
    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        anyhow::bail!("Can't download the file: {}", response.status());
    }

    let text = response.text().await?;

    Ok(text)
}

async fn get_url(date: NaiveDate) -> String {
    format!(
        "https://cbr.ru/scripts/XML_daily.asp?date_req={}",
        date.format("%d/%m/%Y")
    )
}

async fn update_stored_exchange_rates(
    date: &NaiveDate,
    exchange_rates: &HashMap<String, Decimal>,
    pool: &Pool<Postgres>,
    currencies: &Vec<String>,
) -> Result<()> {
    for currency in currencies {
        let rate = exchange_rates.get(currency).ok_or(anyhow!(
            "There is not val_cur for {} at {}",
            &currency,
            &date
        ))?;
        if rate.is_zero() {
            println!("Rate is zero for {} at {}", &currency, &date);
        }
        let reverse_rate = Decimal::ONE / rate;
        let rub = "RUB".to_string();

        set_exchange_rate(date, &rub, currency, rate, pool).await?;
        set_exchange_rate(date, currency, &rub, &reverse_rate, pool).await?;
    }

    Ok(())
}

async fn set_exchange_rate(
    date: &NaiveDate,
    from_currency: &String,
    to_currency: &String,
    rate: &Decimal,
    pool: &Pool<Postgres>,
) -> Result<()> {
    let exchange_rate: Option<ExchangeRate> = sqlx::query_as!(
        ExchangeRate,
        r#"
        SELECT id, from_currency, to_currency, rate, date, created_at, updated_at
        FROM exchange_rates
        WHERE from_currency = $1 AND to_currency = $2 AND date = $3
        "#,
        from_currency,
        to_currency,
        date
    )
    .fetch_optional(pool)
    .await?;

    if let Some(exchange_rate) = exchange_rate {
        if exchange_rate.rate != *rate {
            sqlx::query!(
                r#"
                    UPDATE exchange_rates
                    SET rate = $1, updated_at = NOW()
                    WHERE id = $2
                "#,
                rate,
                exchange_rate.id
            )
            .execute(pool)
            .await?;

            log::info!(
                "Exchange rate updated: {} -> {} at {} = {}",
                from_currency,
                to_currency,
                date,
                rate
            );
        }
    } else {
        sqlx::query!(
            r#"
                INSERT INTO exchange_rates (from_currency, to_currency, rate, date, created_at, updated_at)
                VALUES ($1, $2, $3, $4, NOW(), NOW())
            "#,
            from_currency,
            to_currency,
            rate,
            date
        )
        .execute(pool)
        .await?;

        log::info!(
            "Exchange rate added: {} -> {} at {} = {}",
            from_currency,
            to_currency,
            date,
            rate
        );
    }

    Ok(())
}

async fn get_db_pool() -> Result<Pool<Postgres>> {
    let connection_string = get_connection_string().await?;

    let pool = PgPool::connect(&connection_string).await?;

    Ok(pool)
}

async fn get_connection_string() -> Result<String> {
    let username = env::var("POSTGRES_USER")?;
    let password = env::var("POSTGRES_PASSWORD")?;
    let host = env::var("DB_HOST")?;
    let port = env::var("DB_PORT")?;
    let database = env::var("POSTGRES_DB")?;

    let connection_string = format!(
        "postgres://{}:{}@{}:{}/{}",
        username, password, host, port, database
    );

    Ok(connection_string)
}

fn get_currencies() -> Vec<String> {
    vec!["USD".to_string(), "EUR".to_string()]
}
