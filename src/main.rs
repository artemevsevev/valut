use std::{collections::HashMap, env, str::FromStr, time::Duration};

use actix_web::{App, HttpResponse, HttpServer, Responder, get};
use anyhow::{Result, anyhow};
use chrono::{DateTime, Days, NaiveDate, Timelike, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use sqlx::{PgPool, Pool, Postgres};
use tokio::signal::unix::{SignalKind, signal};
use val_curs::ValCurs;

use crate::exchange_rate::ExchangeRate;

mod exchange_rate;
mod val_curs;

const DELAY_SEC: u64 = 60 * 20;
const RETRYDELAY_SEC: u64 = 5;
const CURRENCIES_VAR: &str = "CURRENCIES";
const DEFAULT_CURRENCIES: &str = "USD,EUR";
const BASE_CURRENCY: &str = "RUB";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let currencies = get_currencies()?;

    start_server().await?;

    log::info!("Valut started");
    log::info!("Currencies: {:?}", currencies);

    tokio::select! {
        _ = async {
            main_loop(&currencies).await;

            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        } => {},

        _ = shutdown_signal() => {
        },
    };

    log::info!("Valut ended");

    Ok(())
}

async fn main_loop(currencies: &[String]) {
    let mut retry_count = 0;
    let mut delay_sec = 0;
    let mut last_execution = DateTime::<Utc>::MIN_UTC;
    let mut last_try = DateTime::<Utc>::MIN_UTC;

    loop {
        let next_execution = last_execution + Duration::from_secs(DELAY_SEC);
        let next_try = last_try + Duration::from_secs(delay_sec);
        let now = Utc::now();

        if (now >= next_execution
            || last_execution.date_naive() != now.date_naive()
            || last_execution.hour() != now.hour())
            && now >= next_try
        {
            last_try = Utc::now();

            match execute(currencies).await {
                Ok(_) => {
                    retry_count = 0;
                    delay_sec = 0;
                    last_execution = Utc::now();
                    last_try = DateTime::<Utc>::MIN_UTC;
                }

                Err(err) => {
                    log::error!("Error executing task: {:?}", err);
                    retry_count += 1;
                    delay_sec = match delay_sec {
                        0 => RETRYDELAY_SEC,
                        n => next_delay(n),
                    };
                    dbg!(retry_count, delay_sec);
                }
            }
        };

        if retry_count > 10 {
            log::error!("Max retries exceeded");
            break;
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn shutdown_signal() -> Result<()> {
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    tokio::select! {
        _ = sigterm.recv() => {
            log::info!("SIGTERM received; starting forced shutdown");
        }
        _ = sigint.recv() => {
            log::info!("SIGINT received; starting forced shutdown");
        }
    }
    Ok(())
}

async fn start_server() -> Result<()> {
    let server = HttpServer::new(|| App::new().service(health))
        .bind("0.0.0.0:8000")?
        .run();

    tokio::spawn(server);

    Ok(())
}

#[get("/health")]
async fn health() -> impl Responder {
    HttpResponse::Ok().body("OK")
}

async fn execute(currencies: &[String]) -> Result<()> {
    let today = Utc::now().date_naive();
    let start_date = today
        .checked_sub_days(Days::new(6))
        .ok_or(anyhow::anyhow!("Can't get previous date for {}", today))?;
    let end_date = today
        .checked_add_days(Days::new(1))
        .ok_or(anyhow::anyhow!("Can't get next date for {}", today))?;

    iterate(start_date, end_date, currencies).await?;

    Ok(())
}

async fn iterate(start_date: NaiveDate, end_date: NaiveDate, currencies: &[String]) -> Result<()> {
    if start_date > end_date {
        return Err(anyhow::anyhow!("Start date must be before end date"));
    }

    let mut current_date = end_date;
    let pool = get_db_pool().await?;

    while current_date >= start_date {
        let exchange_rates = get_exchange_rates_for_date(current_date).await?;

        update_stored_exchange_rates(&current_date, &exchange_rates, &pool, currencies).await?;

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

        if let Some(value) = parse_decimal_string(&normalized_string) {
            map.insert(valute.char_code.clone(), value);
        }
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
    currencies: &[String],
) -> Result<()> {
    for currency in currencies {
        let Some(rate) = exchange_rates.get(currency) else {
            log::warn!("There is no val_cur for {} at {}, skipping", currency, date);
            continue;
        };

        let Some(reverse_rate) = Decimal::ONE.checked_div(*rate) else {
            log::warn!("Rate is zero for {} at {}, skipping", currency, date);
            continue;
        };

        let rub = BASE_CURRENCY.to_string();

        set_exchange_rate(date, currency, &rub, rate, pool).await?;
        set_exchange_rate(date, &rub, currency, &reverse_rate, pool).await?;
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
    let exchange_rate: Option<ExchangeRate> = sqlx::query_as(
        r#"
            SELECT id, rate
            FROM exchange_rates
            WHERE from_currency = $1 AND to_currency = $2 AND date = $3
        "#,
    )
    .bind(&from_currency)
    .bind(&to_currency)
    .bind(date)
    .fetch_optional(pool)
    .await?;

    if let Some(exchange_rate) = exchange_rate {
        if exchange_rate.rate != *rate {
            sqlx::query(
                r#"
                    UPDATE exchange_rates
                    SET rate = $1, updated_at = NOW()
                    WHERE id = $2
                "#,
            )
            .bind(rate)
            .bind(exchange_rate.id)
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
        sqlx::query(
            r#"
                INSERT INTO exchange_rates (from_currency, to_currency, rate, date, created_at, updated_at)
                VALUES ($1, $2, $3, $4, NOW(), NOW())
            "#,
        )
        .bind(from_currency)
        .bind(to_currency)
        .bind(rate)
        .bind(date)
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

fn get_currencies() -> Result<Vec<String>> {
    let raw = env::var(CURRENCIES_VAR).unwrap_or_else(|_| DEFAULT_CURRENCIES.to_string());
    let currencies = parse_currencies(&raw);

    if currencies.is_empty() {
        return Err(anyhow!(
            "{} is set to {:?} but contains no usable currency codes",
            CURRENCIES_VAR,
            raw
        ));
    }

    Ok(currencies)
}

/// Разбирает список валют вида "USD, EUR, KZT": убирает пробелы и пустые
/// элементы, приводит к верхнему регистру, отбрасывает базовую валюту
/// (её ЦБ не котирует) и дубликаты, сохраняя порядок.
fn parse_currencies(raw: &str) -> Vec<String> {
    let mut currencies: Vec<String> = Vec::new();

    for code in raw.split(',') {
        let code = code.trim().to_uppercase();

        if code.is_empty() || code == BASE_CURRENCY || currencies.contains(&code) {
            continue;
        }

        currencies.push(code);
    }

    currencies
}

fn next_delay(value: u64) -> u64 {
    let phi = (1.0 + 5.0_f64.sqrt()) / 2.0;
    (phi * (value as f64)).round() as u64
}

fn parse_decimal_string(s: &str) -> Option<Decimal> {
    // Проверяем наличие научной нотации (e или E)
    if let Some(e_pos) = s.find(['e', 'E']) {
        // Разделяем на мантиссу и экспоненту
        let (mantissa_str, exp_str) = s.split_at(e_pos);
        let exp_str = &exp_str[1..]; // Пропускаем символ 'e' или 'E'

        // Парсим мантиссу и экспоненту
        let mantissa = Decimal::from_str(mantissa_str).ok()?;
        let exponent: i32 = exp_str.parse().ok()?;

        // Вычисляем 10^|exponent|
        let ten = Decimal::from(10);
        let mut power = Decimal::ONE;
        for _ in 0..exponent.abs() {
            power = power.checked_mul(ten)?;
        }

        // Применяем экспоненту
        if exponent >= 0 {
            mantissa.checked_mul(power)
        } else {
            mantissa.checked_div(power)
        }
    } else {
        // Обычный decimal без научной нотации
        Decimal::from_str(s).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_list() {
        assert_eq!(parse_currencies("USD, EUR, KZT"), ["USD", "EUR", "KZT"]);
    }

    #[test]
    fn trims_uppercases_and_drops_empty_items() {
        assert_eq!(parse_currencies(" usd ,, EUR "), ["USD", "EUR"]);
    }

    #[test]
    fn drops_duplicates_keeping_order() {
        assert_eq!(parse_currencies("EUR,USD,eur"), ["EUR", "USD"]);
    }

    #[test]
    fn drops_base_currency() {
        assert_eq!(parse_currencies("RUB,USD"), ["USD"]);
    }

    #[test]
    fn returns_empty_for_blank_input() {
        assert!(parse_currencies("").is_empty());
        assert!(parse_currencies(" , ").is_empty());
        assert!(parse_currencies("RUB").is_empty());
    }

    /// Номинал KZT равен 100, но ЦБ отдаёт `VunitRate` уже за одну единицу,
    /// поэтому делить на номинал не нужно.
    #[tokio::test]
    async fn uses_per_unit_rate_for_currencies_with_nominal_above_one() {
        let xml = r#"
            <ValCurs Date="19.09.2026" name="Foreign Currency Market">
                <Valute ID="R01235">
                    <NumCode>840</NumCode><CharCode>USD</CharCode>
                    <Nominal>1</Nominal><Name>Доллар США</Name>
                    <Value>82,1234</Value><VunitRate>82,1234</VunitRate>
                </Valute>
                <Valute ID="R01335">
                    <NumCode>398</NumCode><CharCode>KZT</CharCode>
                    <Nominal>100</Nominal><Name>Тенге</Name>
                    <Value>18,9387</Value><VunitRate>0,189387</VunitRate>
                </Valute>
            </ValCurs>
        "#;

        let val_curs: ValCurs = quick_xml::de::from_str(xml).unwrap();
        let rates = get_curs_map(&val_curs).await.unwrap();

        assert_eq!(rates["KZT"], Decimal::from_str("0.189387").unwrap());
        assert_eq!(rates["USD"], Decimal::from_str("82.1234").unwrap());
    }

    #[test]
    fn default_list_matches_previous_hardcoded_behaviour() {
        assert_eq!(parse_currencies(DEFAULT_CURRENCIES), ["USD", "EUR"]);
    }
}
