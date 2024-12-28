use anyhow::Result;
use chrono::DateTime;
use comfy_table::presets::UTF8_FULL;
use comfy_table::*;
use mostro_core::dispute::Dispute;
use mostro_core::message::Payload;
use mostro_core::order::{Kind, SmallOrder};

pub fn print_order_preview(ord: Payload) -> Result<String, String> {
    let single_order = match ord {
        Payload::Order(o) => o,
        _ => return Err("Error".to_string()),
    };

    let mut table = Table::new();

    table
        .load_preset(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(160)
        .set_header(vec![
            Cell::new("Buy/Sell")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center),
            Cell::new("Sats Amount")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center),
            Cell::new("Fiat Code")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center),
            Cell::new("Fiat Amount")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center),
            Cell::new("Payment method")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center),
            Cell::new("Premium %")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center),
        ]);

    //Table rows
    let r = Row::from(vec![
        if let Some(k) = single_order.kind {
            match k {
                Kind::Buy => Cell::new(k.to_string())
                    .fg(Color::Green)
                    .set_alignment(CellAlignment::Center),
                Kind::Sell => Cell::new(k.to_string())
                    .fg(Color::Red)
                    .set_alignment(CellAlignment::Center),
            }
        } else {
            Cell::new("BUY/SELL").set_alignment(CellAlignment::Center)
        },
        if single_order.amount == 0 {
            Cell::new("market price").set_alignment(CellAlignment::Center)
        } else {
            Cell::new(single_order.amount).set_alignment(CellAlignment::Center)
        },
        Cell::new(single_order.fiat_code.to_string()).set_alignment(CellAlignment::Center),
        // No range order print row
        if single_order.min_amount.is_none() && single_order.max_amount.is_none() {
            Cell::new(single_order.fiat_amount.to_string()).set_alignment(CellAlignment::Center)
        } else {
            let range_str = format!(
                "{}-{}",
                single_order.min_amount.unwrap(),
                single_order.max_amount.unwrap()
            );
            Cell::new(range_str).set_alignment(CellAlignment::Center)
        },
        Cell::new(single_order.payment_method.to_string()).set_alignment(CellAlignment::Center),
        Cell::new(single_order.premium.to_string()).set_alignment(CellAlignment::Center),
    ]);

    table.add_row(r);

    Ok(table.to_string())
}

pub fn print_orders_table(orders_table: Vec<SmallOrder>) -> Result<String> {
    let mut table = Table::new();

    //Table rows
    let mut rows: Vec<Row> = Vec::new();

    if orders_table.is_empty() {
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_width(160)
            .set_header(vec![Cell::new("Sorry...")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center)]);

        // Single row for error
        let mut r = Row::new();

        r.add_cell(
            Cell::new("No offers found with requested parameters...")
                .fg(Color::Red)
                .set_alignment(CellAlignment::Center),
        );

        //Push single error row
        rows.push(r);
    } else {
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_width(160)
            .set_header(vec![
                Cell::new("Buy/Sell")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Order Id")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Status")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Amount")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Fiat Code")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Fiat Amount")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Payment method")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Created")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
            ]);

        //Iterate to create table of orders
        for single_order in orders_table.into_iter() {
            let date = DateTime::from_timestamp(single_order.created_at.unwrap_or(0), 0);

            let r = Row::from(vec![
                if let Some(k) = single_order.kind {
                    match k {
                        Kind::Buy => Cell::new(k.to_string())
                            .fg(Color::Green)
                            .set_alignment(CellAlignment::Center),
                        Kind::Sell => Cell::new(k.to_string())
                            .fg(Color::Red)
                            .set_alignment(CellAlignment::Center),
                    }
                } else {
                    Cell::new("BUY/SELL").set_alignment(CellAlignment::Center)
                },
                Cell::new(single_order.id.unwrap()).set_alignment(CellAlignment::Center),
                Cell::new(single_order.status.unwrap().to_string())
                    .set_alignment(CellAlignment::Center),
                if single_order.amount == 0 {
                    Cell::new("market price").set_alignment(CellAlignment::Center)
                } else {
                    Cell::new(single_order.amount.to_string()).set_alignment(CellAlignment::Center)
                },
                Cell::new(single_order.fiat_code.to_string()).set_alignment(CellAlignment::Center),
                // No range order print row
                if single_order.min_amount.is_none() && single_order.max_amount.is_none() {
                    Cell::new(single_order.fiat_amount.to_string())
                        .set_alignment(CellAlignment::Center)
                } else {
                    let range_str = format!(
                        "{}-{}",
                        single_order.min_amount.unwrap(),
                        single_order.max_amount.unwrap()
                    );
                    Cell::new(range_str).set_alignment(CellAlignment::Center)
                },
                Cell::new(single_order.payment_method.to_string())
                    .set_alignment(CellAlignment::Center),
                Cell::new(date.unwrap()),
            ]);
            rows.push(r);
        }
    }

    table.add_rows(rows);

    Ok(table.to_string())
}

pub fn print_disputes_table(disputes_table: Vec<Dispute>) -> Result<String> {
    let mut table = Table::new();

    //Table rows
    let mut rows: Vec<Row> = Vec::new();

    if disputes_table.is_empty() {
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_width(160)
            .set_header(vec![Cell::new("Sorry...")
                .add_attribute(Attribute::Bold)
                .set_alignment(CellAlignment::Center)]);

        // Single row for error
        let mut r = Row::new();

        r.add_cell(
            Cell::new("No disputes found with requested parameters...")
                .fg(Color::Red)
                .set_alignment(CellAlignment::Center),
        );

        //Push single error row
        rows.push(r);
    } else {
        table
            .load_preset(UTF8_FULL)
            .set_content_arrangement(ContentArrangement::Dynamic)
            .set_width(160)
            .set_header(vec![
                Cell::new("Dispute Id")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Status")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
                Cell::new("Created")
                    .add_attribute(Attribute::Bold)
                    .set_alignment(CellAlignment::Center),
            ]);

        //Iterate to create table of orders
        for single_dispute in disputes_table.into_iter() {
            let date = DateTime::from_timestamp(single_dispute.created_at, 0);

            let r = Row::from(vec![
                Cell::new(single_dispute.id).set_alignment(CellAlignment::Center),
                Cell::new(single_dispute.status.to_string()).set_alignment(CellAlignment::Center),
                Cell::new(date.unwrap()),
            ]);
            rows.push(r);
        }
    }

    table.add_rows(rows);

    Ok(table.to_string())
}
