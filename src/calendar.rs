use crate::core::ClockState;

pub const MONTH_NAMES: [&str; 12] = [
    "janeiro",
    "fevereiro",
    "março",
    "abril",
    "maio",
    "junho",
    "julho",
    "agosto",
    "setembro",
    "outubro",
    "novembro",
    "dezembro",
];

pub const WEEKDAY_NAMES: [&str; 7] = [
    "domingo",
    "segunda-feira",
    "terça-feira",
    "quarta-feira",
    "quinta-feira",
    "sexta-feira",
    "sábado",
];

pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Sunday-first weekday for the first day of a Gregorian month.
pub fn weekday_of_first(year: i32, month: u8) -> u8 {
    let adjusted_year = year - i32::from(month <= 2);
    let adjusted_month = if month <= 2 { month + 12 } else { month };
    let century = adjusted_year.rem_euclid(100);
    let zero_based_century = adjusted_year.div_euclid(100);
    ((1 + (13 * (i32::from(adjusted_month) + 1)) / 5
        + century
        + century / 4
        + zero_based_century / 4
        + 5 * zero_based_century)
        .rem_euclid(7) as u8
        + 6)
        % 7
}

pub fn shift_month(year: i32, month: u8, delta: i8) -> (i32, u8) {
    let index = year
        .saturating_mul(12)
        .saturating_add(i32::from(month.saturating_sub(1)))
        .saturating_add(i32::from(delta));
    (index.div_euclid(12), (index.rem_euclid(12) + 1) as u8)
}

pub fn grid_rows(year: i32, month: u8) -> u8 {
    let cells = u16::from(weekday_of_first(year, month)) + u16::from(days_in_month(year, month));
    cells.div_ceil(7) as u8
}

pub fn grid_day(index: usize, leading: usize, days: usize) -> Option<u8> {
    if index < leading {
        return None;
    }

    let day = index - leading + 1;
    (day <= days).then_some(day as u8)
}

pub fn grid_days(year: i32, month: u8) -> Vec<Option<u8>> {
    let leading = usize::from(weekday_of_first(year, month));
    let days = usize::from(days_in_month(year, month));
    let rows = usize::from(grid_rows(year, month));
    (0..rows * 7)
        .map(|index| grid_day(index, leading, days))
        .collect()
}

pub fn format_clock(clock: &ClockState) -> String {
    format!(
        "{} {:02}/{:02} {:02}:{:02}",
        WEEKDAY_NAMES
            .get(usize::from(clock.weekday))
            .copied()
            .unwrap_or("domingo"),
        clock.day,
        clock.month,
        clock.hour,
        clock.minute
    )
}

pub fn format_month(year: i32, month: u8) -> String {
    let name = MONTH_NAMES
        .get(usize::from(month.saturating_sub(1)))
        .copied()
        .unwrap_or("?");
    let mut chars = name.chars();
    let title = chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default();
    format!("{title} {year}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portuguese_weekday_formatting_is_full_and_localized() {
        let expected = [
            "domingo",
            "segunda-feira",
            "terça-feira",
            "quarta-feira",
            "quinta-feira",
            "sexta-feira",
            "sábado",
        ];
        assert_eq!(WEEKDAY_NAMES, expected);
        assert_eq!(
            format_clock(&ClockState {
                year: 2026,
                weekday: 1,
                hour: 11,
                minute: 52,
                day: 21,
                month: 9,
            }),
            "segunda-feira 21/09 11:52"
        );
    }

    #[test]
    fn month_lengths_cover_leap_normal_thirty_and_thirty_one_day_months() {
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2025, 2), 28);
        assert_eq!(days_in_month(2025, 4), 30);
        assert_eq!(days_in_month(2025, 1), 31);
    }

    #[test]
    fn navigation_crosses_year_boundaries() {
        assert_eq!(shift_month(2026, 1, -1), (2025, 12));
        assert_eq!(shift_month(2026, 12, 1), (2027, 1));
    }

    #[test]
    fn sunday_first_grid_placement_is_deterministic() {
        assert_eq!(weekday_of_first(2026, 2), 0);
        assert_eq!(grid_days(2026, 2).first(), Some(&Some(1)));
        assert_eq!(grid_rows(2026, 2), 4);
        assert_eq!(grid_rows(2026, 8), 6);
    }

    #[test]
    fn grid_leading_cells_are_empty_for_each_weekday_start() {
        assert_eq!(weekday_of_first(2026, 2), 0);
        assert_eq!(grid_days(2026, 2).first().copied(), Some(Some(1)));

        assert_eq!(weekday_of_first(2026, 6), 1);
        assert_eq!(grid_days(2026, 6).get(..2), Some(&[None, Some(1)][..]));

        assert_eq!(weekday_of_first(2026, 9), 2);
        assert_eq!(
            grid_days(2026, 9).get(..3),
            Some(&[None, None, Some(1)][..])
        );

        assert_eq!(weekday_of_first(2026, 8), 6);
        assert_eq!(
            grid_days(2026, 8).get(..7),
            Some(&[None, None, None, None, None, None, Some(1)][..])
        );
    }

    #[test]
    fn grid_trailing_cells_are_empty_for_all_month_lengths() {
        for (year, month, last_day) in [(2025, 2, 28), (2024, 2, 29), (2025, 4, 30), (2025, 1, 31)]
        {
            let grid = grid_days(year, month);
            let last_index = grid
                .iter()
                .rposition(|day| *day == Some(last_day))
                .expect("last day must be present");
            assert!(grid[last_index + 1..].iter().all(Option::is_none));
        }
    }
}
