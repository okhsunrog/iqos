use iqos::{Iqos, IqosUsb, ProductNumberKind};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let usb = IqosUsb::connect().await?;
    println!("Connected: {}", usb.location());
    println!("Model: {:?}", usb.model());
    println!("USB metadata: {:?}", usb.device_info());

    let iqos = Iqos::new(usb);
    let product_number = iqos.read_product_number(ProductNumberKind::Stick).await?;
    println!("Product number: {product_number}");

    let battery_voltage = iqos.read_battery_voltage().await?;
    println!("Battery voltage: {battery_voltage:.3} V");

    let diagnosis = iqos.read_diagnosis().await?;
    println!(
        "Diagnosis: puffs={:?} days={:?} battery={:?}",
        diagnosis.total_smoking_count, diagnosis.days_used, diagnosis.battery_voltage
    );

    Ok(())
}
