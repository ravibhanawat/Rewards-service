//! Money arithmetic. All amounts are integer paise (1 INR = 100 paise); no
//! floating point is used anywhere in pricing.

pub type Paise = i64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceBreakdown {
    pub subtotal: Paise,
    pub discount: Paise,
    pub total: Paise,
}

pub fn line_total(unit_price: Paise, quantity: i32) -> Result<Paise, Overflow> {
    unit_price.checked_mul(i64::from(quantity)).ok_or(Overflow)
}

/// `percent`% of `subtotal`, rounded half up to the nearest paisa.
///
/// The discount is computed once on the order subtotal (never per line), so
/// there is exactly one rounding step per order. Intermediate math is i128,
/// so it cannot overflow, and the result never exceeds the subtotal.
pub fn percentage_discount(subtotal: Paise, percent: i32) -> Paise {
    debug_assert!(subtotal >= 0 && (0..=100).contains(&percent));
    let subtotal = i128::from(subtotal.max(0));
    let percent = i128::from(percent.clamp(0, 100));
    let rounded = (subtotal * percent + 50) / 100;
    // Lossless: rounded <= subtotal <= i64::MAX because percent <= 100.
    rounded.min(subtotal) as Paise
}

/// Price an order from `(unit_price, quantity)` lines and an optional coupon.
pub fn price_order(
    lines: &[(Paise, i32)],
    discount_percent: Option<i32>,
) -> Result<PriceBreakdown, Overflow> {
    let mut subtotal: Paise = 0;
    for &(unit_price, quantity) in lines {
        subtotal = subtotal
            .checked_add(line_total(unit_price, quantity)?)
            .ok_or(Overflow)?;
    }
    let discount = discount_percent.map_or(0, |pct| percentage_discount(subtotal, pct));
    Ok(PriceBreakdown {
        subtotal,
        discount,
        total: subtotal - discount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discount_rounds_half_up_to_the_paisa() {
        assert_eq!(percentage_discount(1000, 10), 100); // exact
        assert_eq!(percentage_discount(999, 10), 100); // 99.9  -> 100
        assert_eq!(percentage_discount(994, 10), 99); //  99.4  -> 99
        assert_eq!(percentage_discount(995, 10), 100); // 99.5  -> 100 (tie goes up)
        assert_eq!(percentage_discount(1010, 15), 152); // 151.5 -> 152
        assert_eq!(percentage_discount(1, 10), 0); //     0.1   -> 0
        assert_eq!(percentage_discount(5, 10), 1); //     0.5   -> 1
    }

    #[test]
    fn discount_never_exceeds_subtotal_or_goes_negative() {
        assert_eq!(percentage_discount(0, 10), 0);
        assert_eq!(percentage_discount(12_345, 100), 12_345);
        assert_eq!(percentage_discount(i64::MAX, 100), i64::MAX);
        let max = price_order(&[(i64::MAX, 1)], Some(100)).unwrap();
        assert_eq!((max.discount, max.total), (i64::MAX, 0));
    }

    #[test]
    fn order_pricing_reconciles() {
        let p = price_order(&[(349_900, 2), (4_999, 3)], Some(10)).unwrap();
        assert_eq!(p.subtotal, 349_900 * 2 + 4_999 * 3); // 714_797
        assert_eq!(p.discount, 71_480); // 71_479.7 -> 71_480
        assert_eq!(p.total, p.subtotal - p.discount);

        let none = price_order(&[(4_999, 3)], None).unwrap();
        assert_eq!((none.discount, none.total), (0, 14_997));
        assert_eq!(
            price_order(&[], Some(10)).unwrap(),
            PriceBreakdown {
                subtotal: 0,
                discount: 0,
                total: 0
            }
        );
    }

    #[test]
    fn overflow_is_reported_not_wrapped() {
        assert_eq!(line_total(i64::MAX, 2), Err(Overflow));
        assert_eq!(price_order(&[(i64::MAX, 1), (1, 1)], None), Err(Overflow));
    }
}
