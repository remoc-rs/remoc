// Compile-time benchmark: remoc channels with many different types.
//
// This benchmark creates mpsc, oneshot, and watch channels for 20 different
// complex types to measure the monomorphization cost of remoc channel infrastructure.

mod types;

use types::*;

// Create channels for each type to force monomorphization.
// We use `black_box` to prevent the compiler from optimizing away unused channels.
fn create_mpsc_channels() {
    macro_rules! chan {
        ($ty:ty) => {{
            let (tx, rx) = remoc::rch::mpsc::channel::<$ty, remoc::codec::Default>(16);
            std::hint::black_box((tx, rx));
        }};
    }

    chan!(Simple);
    chan!(Color);
    chan!(Address);
    chan!(Person);
    chan!(Company);
    chan!(Message);
    chan!(Config);
    chan!(ConfigEntry);
    chan!(ConfigValue);
    chan!(Matrix);
    chan!(TimeSeries);
    chan!(Document);
    chan!(Section);
    chan!(Figure);
    chan!(Event);
    chan!(EventKind);
    chan!(ApiResponse);
    chan!(ResponseBody);
    chan!(DatabaseRecord);
    chan!(Workspace);
}

fn create_watch_channels() {
    macro_rules! chan {
        ($ty:ty, $val:expr) => {{
            let (tx, rx) = remoc::rch::watch::channel::<$ty, remoc::codec::Default>($val);
            std::hint::black_box((tx, rx));
        }};
    }

    chan!(Simple, Simple {
        a: 0, b: 0, c: 0, d: 0, e: 0, f: 0, g: 0, h: 0,
        i: 0.0, j: 0.0, k: false, l: String::new(), m: Vec::new()
    });
    chan!(Color, Color::Red);
    chan!(Address, Address {
        street: String::new(), city: String::new(), state: String::new(),
        zip: String::new(), country: String::new()
    });
    chan!(u32, 0u32);
    chan!(String, String::new());
    chan!(Vec<u8>, Vec::new());
    chan!(Option<String>, None);
    chan!(bool, false);
    chan!(f64, 0.0);
    chan!(i64, 0i64);
    chan!(u64, 0u64);
    chan!(Matrix, Matrix { rows: 0, cols: 0, data: Vec::new() });
    chan!(ConfigValue, ConfigValue::Bool(false));
    chan!(Figure, Figure { caption: String::new(), data: Vec::new(), width: 0, height: 0 });
    chan!(Vec<String>, Vec::new());
}

fn create_oneshot_channels() {
    macro_rules! chan {
        ($ty:ty) => {{
            let (tx, rx) = remoc::rch::oneshot::channel::<$ty, remoc::codec::Default>();
            std::hint::black_box((tx, rx));
        }};
    }

    chan!(Simple);
    chan!(Color);
    chan!(Address);
    chan!(Person);
    chan!(Company);
    chan!(Message);
    chan!(Config);
    chan!(ConfigEntry);
    chan!(ConfigValue);
    chan!(Matrix);
    chan!(TimeSeries);
    chan!(Document);
    chan!(Section);
    chan!(Figure);
    chan!(Event);
    chan!(EventKind);
    chan!(ApiResponse);
    chan!(ResponseBody);
    chan!(DatabaseRecord);
    chan!(Workspace);
}

#[tokio::main]
async fn main() {
    create_mpsc_channels();
    create_watch_channels();
    create_oneshot_channels();
    println!("remoc channels compile-time benchmark OK");
}
