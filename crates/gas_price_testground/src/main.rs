use plotters::prelude::*;
use std::cmp::max;
struct Algorithm {
    amount: u64,
    min: u64,
}

impl Algorithm {
    fn calculate_gas_price(
        &self,
        old_gas_price: u64,
        total_production_reward: u64,
        total_da_recording_cost: u64,
        used: u64,
        capacity: u64,
    ) -> u64 {
        if total_da_recording_cost > total_production_reward {
            old_gas_price + self.amount
        } else {
            if used > capacity / 2 {
                old_gas_price + self.amount
            } else {
                max(old_gas_price.saturating_sub(self.amount), self.min)
            }
        }
    }
}

fn main() {
    let da_recording_cost = (0u32..1000)
        .map(|val| f64::try_from(val).unwrap() / 100.)
        .map(f64::sin)
        .map(|x| x * 10_000. + 20_000.)
        .map(|x| x as u64);
    let amount = 1;
    let min = 10;
    let algo = Algorithm { amount, min };
    let mut gas_price = 100;
    let mut gas_prices = vec![gas_price as i32];
    let mut profit = vec![0i32];
    let mut total_cost = 0;
    let mut total_reward = 0;
    let gas_spent = 200;
    // 50% capacity
    let used = gas_spent;
    let capacity = gas_spent * 2;
    for cost in da_recording_cost {
        total_cost += cost;
        gas_price =
            algo.calculate_gas_price(gas_price, total_reward, total_cost, used, capacity);
        gas_prices.push(gas_price as i32);

        let new_gas_reward = gas_price * gas_spent;
        total_reward += new_gas_reward;
        let total_profit = total_reward as i32 - total_cost as i32;
        profit.push(total_profit as i32);
    }

    // Plotting code starts here
    let root = BitMapBackend::new("gas_prices.png", (640, 480)).into_drawing_area();
    root.fill(&WHITE).unwrap();

    let mut chart = ChartBuilder::on(&root)
        .caption("Gas Prices Over Time", ("sans-serif", 50).into_font())
        .margin(5)
        .x_label_area_size(40)
        .y_label_area_size(60)
        .right_y_label_area_size(40)
        .build_cartesian_2d(0..profit.len(), 0..*profit.iter().max().unwrap())
        .unwrap()
        .set_secondary_coord(0..gas_prices.len(), 0..*gas_prices.iter().max().unwrap());

    chart
        .configure_mesh()
        .y_desc("Profit")
        .x_desc("Block")
        .draw()
        .unwrap();

    chart
        .configure_secondary_axes()
        .y_desc("Gas Price")
        .draw()
        .unwrap();

    const PRICE_COLOR: RGBColor = RED;
    const PROFIT_COLOR: RGBColor = BLUE;

    // Add chart for profits
    chart
        .draw_series(LineSeries::new(
            profit.iter().enumerate().map(|(x, y)| (x, *y)),
            &PROFIT_COLOR,
        ))
        .unwrap()
        .label("Profit")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &PROFIT_COLOR));

    chart
        .draw_secondary_series(LineSeries::new(
            gas_prices.iter().enumerate().map(|(x, y)| (x, *y)),
            &PRICE_COLOR,
        ))
        .unwrap()
        .label("Gas Price")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], &PRICE_COLOR));

    chart
        .configure_series_labels()
        .background_style(&WHITE.mix(0.8))
        .border_style(&BLACK)
        .draw()
        .unwrap();

    root.present().unwrap();
}
