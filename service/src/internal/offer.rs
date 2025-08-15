use boundless_market::{alloy::primitives::utils::parse_ether, request_builder::OfferParams};
use std::env;

/// Helper function to create an [OfferParams] from env vars.
/// This panics rather than error handling as failures here can only be caused by a misconfigured env
/// which should be caught at startup.
pub(crate) fn offer_from_env() -> OfferParams {
    OfferParams::builder()
        .min_price(
            parse_ether(
                &env::var("ORDER_MIN_PRICE")
                    .expect("ORDER_MIN_PRICE must be set to interact with Boundless market"),
            )
            .expect("Failed to parse min price"),
        )
        .max_price(
            parse_ether(
                &env::var("ORDER_MAX_PRICE")
                    .expect("ORDER_MAX_PRICE must be set to interact with Boundless market"),
            )
            .expect("Failed to parse max price"),
        )
        .timeout(
            u32::from_str_radix(
                &env::var("ORDER_TIMEOUT")
                    .expect("ORDER_TIMEOUT must be set to interact with Boundless market"),
                10,
            )
            .expect("ORDER_TIMEOUT could not be parsed as u32"),
        )
        .lock_timeout(
            u32::from_str_radix(
                &env::var("ORDER_LOCK_TIMEOUT")
                    .expect("ORDER_LOCK_TIMEOUT must be set to interact with Boundless market"),
                10,
            )
            .expect("ORDER_LOCK_TIMEOUT could not be parsed as u32"),
        )
        .ramp_up_period(
            u32::from_str_radix(
                &env::var("ORDER_RAMP_UP_PERIOD")
                    .expect("ORDER_RAMP_UP_PERIOD must be set to interact with Boundless market"),
                10,
            )
            .expect("ORDER_RAMP_UP_PERIOD could not be parsed as u32"),
        )
        .build()
        .expect("Failed to build OfferParams")
}
