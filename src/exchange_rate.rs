use rust_decimal::Decimal;
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
pub struct ExchangeRate {
    pub id: Uuid,
    pub rate: Decimal,
}
