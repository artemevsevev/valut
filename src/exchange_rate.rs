use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(Debug)]
pub struct ExchangeRate {
    pub id: Uuid,
    pub rate: Decimal,
}
