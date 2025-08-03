use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(Debug)]
pub struct ExchangeRate {
    pub id: Uuid,
    pub from_currency: String,
    pub to_currency: String,
    pub rate: Decimal,
    pub date: NaiveDate,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}
