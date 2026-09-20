use std::{collections::HashMap, env, str::FromStr, time::Duration};

use actix_web::{App, HttpResponse, HttpServer, Responder, get};
use anyhow::{Result, anyhow};
use chrono::{Days, NaiveDate, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use sqlx::{Pool, Postgres, postgres::PgPoolOptions};
use tokio::{
    signal::unix::{SignalKind, signal},
    time::MissedTickBehavior,
};
use val_curs::ValCurs;

use crate::exchange_rate::ExchangeRate;

mod exchange_rate;
mod val_curs;

const PERIOD: Duration = Duration::from_secs(60 * 20);
const RETRY_DELAY: Duration = Duration::from_secs(5);
const MAX_RETRIES: u32 = 10;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const CURRENCIES_VAR: &str = "CURRENCIES";
const DEFAULT_CURRENCIES: &str = "USD,EUR";
const BASE_CURRENCY: &str = "RUB";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let currencies = get_currencies()?;
    let pool = get_db_pool()?;
    let client = get_http_client()?;

    start_server().await?;

    log::info!("Valut started");
    log::info!("Currencies: {:?}", currencies);

    tokio::select! {
        _ = main_loop(&currencies, &pool, &client) => {},

        _ = shutdown_signal() => {},
    };

    log::info!("Valut ended");

    Ok(())
}

/// Раз в [`PERIOD`] перечитывает окно дат у ЦБ. Первый тик срабатывает сразу,
/// поэтому прогон происходит и при старте сервиса.
async fn main_loop(currencies: &[String], pool: &Pool<Postgres>, client: &Client) -> ! {
    let mut ticker = tokio::time::interval(PERIOD);
    // Затянувшийся прогон не должен приводить к очереди пропущенных тиков,
    // которые дефолтный `Burst` выстрелил бы подряд.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;

        if let Err(err) = execute_with_retries(currencies, pool, client).await {
            log::error!("Max retries exceeded, waiting for next tick: {:?}", err);
        }

        // Одного `Delay` мало: пропущенный тик он отдаёт немедленно, поэтому
        // после прогона длиннее PERIOD следующий начался бы без паузы. `reset`
        // переносит следующий тик на PERIOD от момента завершения прогона.
        ticker.reset();
    }
}

async fn execute_with_retries(
    currencies: &[String],
    pool: &Pool<Postgres>,
    client: &Client,
) -> Result<()> {
    let mut delay = RETRY_DELAY;

    for attempt in 1..=MAX_RETRIES {
        match execute(currencies, pool, client).await {
            Ok(()) => return Ok(()),

            Err(err) if attempt == MAX_RETRIES => return Err(err),

            Err(err) => {
                log::warn!(
                    "Attempt {}/{} failed: {:?}; retrying in {:?}",
                    attempt,
                    MAX_RETRIES,
                    err,
                    delay
                );

                tokio::time::sleep(delay).await;
                delay = next_delay(delay);
            }
        }
    }

    unreachable!("the loop returns on the last attempt")
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

async fn execute(currencies: &[String], pool: &Pool<Postgres>, client: &Client) -> Result<()> {
    let today = Utc::now().date_naive();
    let start_date = today
        .checked_sub_days(Days::new(6))
        .ok_or(anyhow::anyhow!("Can't get previous date for {}", today))?;
    let end_date = today
        .checked_add_days(Days::new(1))
        .ok_or(anyhow::anyhow!("Can't get next date for {}", today))?;

    iterate(start_date, end_date, currencies, pool, client).await?;

    Ok(())
}

async fn iterate(
    start_date: NaiveDate,
    end_date: NaiveDate,
    currencies: &[String],
    pool: &Pool<Postgres>,
    client: &Client,
) -> Result<()> {
    if start_date > end_date {
        return Err(anyhow::anyhow!("Start date must be before end date"));
    }

    let mut current_date = end_date;

    while current_date >= start_date {
        let exchange_rates = get_exchange_rates_for_date(current_date, client).await?;

        update_stored_exchange_rates(&current_date, &exchange_rates, pool, currencies).await?;

        current_date = current_date
            .pred_opt()
            .ok_or(anyhow::anyhow!("Can't get pred date for {}", current_date))?;
    }

    Ok(())
}

async fn get_exchange_rates_for_date(
    date: NaiveDate,
    client: &Client,
) -> Result<HashMap<String, Decimal>> {
    let val_curs = get_val_curs(date, client).await?;

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

async fn get_val_curs(date: NaiveDate, client: &Client) -> Result<ValCurs> {
    let url = get_url(date).await;
    let text = load_xml(&url, client).await?;
    let val_curs: ValCurs = quick_xml::de::from_str(&text)?;

    Ok(val_curs)
}

async fn load_xml(url: &str, client: &Client) -> Result<String> {
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
    let mut quotes: Vec<(&str, Decimal)> = vec![(BASE_CURRENCY, Decimal::ONE)];

    for currency in currencies {
        let Some(rate) = exchange_rates.get(currency) else {
            log::warn!("There is no val_cur for {} at {}, skipping", currency, date);
            continue;
        };

        if *rate == Decimal::ZERO {
            log::warn!("Rate is zero for {} at {}, skipping", currency, date);
            continue;
        }

        quotes.push((currency.as_str(), *rate));
    }

    for (from_currency, to_currency, rate) in rate_pairs(&quotes) {
        set_exchange_rate(date, from_currency, to_currency, &rate, pool).await?;
    }

    Ok(())
}

/// Все упорядоченные пары валют: курс `from -> to` — сколько единиц `to` дают
/// за одну единицу `from`. ЦБ кросс-курсы не публикует, поэтому они считаются
/// через рубль. Рубль при этом сам присутствует в `quotes` с курсом 1, так что
/// пары с ним получаются той же формулой, что и кросс-курсы, и остаются
/// такими же, как до появления кросс-курсов.
fn rate_pairs<'a>(quotes: &[(&'a str, Decimal)]) -> Vec<(&'a str, &'a str, Decimal)> {
    let mut pairs = Vec::with_capacity(quotes.len() * quotes.len().saturating_sub(1));

    for (from_currency, from_rate) in quotes {
        for (to_currency, to_rate) in quotes {
            if from_currency == to_currency {
                continue;
            }

            // Нулевые курсы отсеяны при сборе `quotes`, поэтому `None` здесь —
            // это переполнение. Молча терять такую пару не стоит.
            let Some(rate) = from_rate.checked_div(*to_rate) else {
                log::warn!(
                    "Can't compute rate {} -> {}, skipping",
                    from_currency,
                    to_currency
                );
                continue;
            };

            pairs.push((*from_currency, *to_currency, rate));
        }
    }

    pairs
}

async fn set_exchange_rate(
    date: &NaiveDate,
    from_currency: &str,
    to_currency: &str,
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
    .bind(from_currency)
    .bind(to_currency)
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

/// Пул создаётся лениво: сеть не трогается до первого запроса, поэтому старт
/// сервиса раньше БД не роняет процесс, а остаётся ретраибельной ошибкой
/// внутри [`main_loop`].
fn get_db_pool() -> Result<Pool<Postgres>> {
    let connection_string = get_connection_string()?;

    let pool = PgPoolOptions::new().connect_lazy(&connection_string)?;

    Ok(pool)
}

fn get_http_client() -> Result<Client> {
    let client = Client::builder().timeout(HTTP_TIMEOUT).build()?;

    Ok(client)
}

fn get_connection_string() -> Result<String> {
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

/// Следующая пауза между ретраями: золотое сечение, округлённое до целых
/// секунд — 5, 8, 13, 21, 34, 55, 89, 144, 233.
fn next_delay(value: Duration) -> Duration {
    let phi = (1.0 + 5.0_f64.sqrt()) / 2.0;

    Duration::from_secs((phi * value.as_secs_f64()).round() as u64)
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

    /// Котировки к рублю в том виде, в каком их собирает
    /// [`update_stored_exchange_rates`]: рубль первым с курсом 1, далее валюты
    /// в порядке `CURRENCIES`. Значения USD и KZT — те же `VunitRate`, что и в
    /// XML-фикстуре теста на номинал.
    fn quotes_fixture() -> Vec<(&'static str, Decimal)> {
        vec![
            (BASE_CURRENCY, Decimal::ONE),
            ("USD", Decimal::from_str("82.1234").unwrap()),
            ("EUR", Decimal::from_str("96.5432").unwrap()),
            ("KZT", Decimal::from_str("0.189387").unwrap()),
        ]
    }

    fn rate_of(pairs: &[(&str, &str, Decimal)], from: &str, to: &str) -> Decimal {
        pairs
            .iter()
            .find(|(pair_from, pair_to, _)| *pair_from == from && *pair_to == to)
            .unwrap_or_else(|| panic!("no pair {} -> {}", from, to))
            .2
    }

    /// Кросс-курс считается из `VunitRate`, которые уже приведены к одной
    /// единице. Делить сырые `Value` нельзя: у KZT номинал 100, и результат
    /// оказался бы в сто раз меньше — 4.34 вместо 433.63.
    #[test]
    fn cross_rate_uses_per_unit_quotes() {
        let pairs = rate_pairs(&quotes_fixture());

        assert_eq!(
            rate_of(&pairs, "USD", "KZT").round_dp(4),
            Decimal::from_str("433.6274").unwrap()
        );
    }

    #[test]
    fn cross_rate_is_written_for_both_directions() {
        let pairs = rate_pairs(&quotes_fixture());
        let usd = Decimal::from_str("82.1234").unwrap();
        let kzt = Decimal::from_str("0.189387").unwrap();

        assert_eq!(rate_of(&pairs, "KZT", "USD"), kzt.checked_div(usd).unwrap());
        assert_eq!(
            (rate_of(&pairs, "USD", "KZT") * rate_of(&pairs, "KZT", "USD")).round_dp(10),
            Decimal::ONE
        );
    }

    /// Появление кросс-курсов не должно менять уже записанные пары с рублём.
    #[test]
    fn base_currency_pairs_keep_previous_values() {
        let pairs = rate_pairs(&quotes_fixture());
        let usd = Decimal::from_str("82.1234").unwrap();

        assert_eq!(rate_of(&pairs, "USD", BASE_CURRENCY), usd);
        assert_eq!(
            rate_of(&pairs, BASE_CURRENCY, "USD"),
            Decimal::ONE.checked_div(usd).unwrap()
        );
    }

    #[test]
    fn covers_every_ordered_pair_without_self_pairs() {
        let pairs = rate_pairs(&quotes_fixture());

        assert_eq!(pairs.len(), 12);
        assert!(pairs.iter().all(|(from, to, _)| from != to));

        let mut codes: Vec<&str> = pairs.iter().map(|(from, _, _)| *from).collect();
        codes.sort_unstable();
        codes.dedup();

        assert_eq!(codes, ["EUR", "KZT", "RUB", "USD"]);
    }

    /// Валюта, которой нет в ответе ЦБ, не попадает в `quotes` — вместе с ней
    /// исчезают только её пары, остальные считаются как обычно.
    #[test]
    fn skips_pairs_of_unavailable_currency() {
        let quotes: Vec<(&str, Decimal)> = quotes_fixture()
            .into_iter()
            .filter(|(code, _)| *code != "EUR")
            .collect();

        let pairs = rate_pairs(&quotes);

        assert_eq!(pairs.len(), 6);
        assert!(
            pairs
                .iter()
                .all(|(from, to, _)| *from != "EUR" && *to != "EUR")
        );
        assert_eq!(
            rate_of(&pairs, "USD", "KZT").round_dp(4),
            Decimal::from_str("433.6274").unwrap()
        );
        assert_eq!(
            rate_of(&pairs, BASE_CURRENCY, "USD"),
            Decimal::ONE
                .checked_div(Decimal::from_str("82.1234").unwrap())
                .unwrap()
        );
    }

    #[test]
    fn backoff_follows_whole_second_golden_ratio() {
        let mut delay = RETRY_DELAY;
        let mut sequence = vec![delay.as_secs()];

        for _ in 1..9 {
            delay = next_delay(delay);
            sequence.push(delay.as_secs());
        }

        assert_eq!(sequence, [5, 8, 13, 21, 34, 55, 89, 144, 233]);
    }
}
