use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub type FiatNames = HashMap<String, FiatNamesValue>;
pub type FiatList = Vec<(String, String)>;

#[derive(Debug, Serialize, Deserialize)]
pub struct FiatNamesValue {
    #[serde(rename = "symbol")]
    symbol: String,

    #[serde(rename = "name")]
    name: String,

    #[serde(rename = "symbol_native")]
    symbol_native: String,

    #[serde(rename = "decimal_digits")]
    decimal_digits: i64,

    #[serde(rename = "rounding")]
    rounding: f64,

    #[serde(rename = "code")]
    code: String,

    #[serde(rename = "emoji")]
    emoji: String,

    #[serde(rename = "name_plural")]
    name_plural: String,

    #[serde(rename = "price")]
    price: Option<bool>,

    #[serde(rename = "locale")]
    locale: Option<String>,
}

pub fn check_currency_ticker(currency: String) -> Option<String> {
    let upper = currency.to_uppercase();
    let mut selectedcurrency: Option<String> = None;
    let mut description = String::new();

    let list = load_fiat_values();

    for curr in list.iter() {
        if curr.0 == upper {
            selectedcurrency = Some(curr.0.to_owned());
            description = curr.1.to_owned();
        }
    }

    match selectedcurrency.clone() {
        Some(s) => println!("You have selected all offers of {} - {}", s, description),
        None => println!("Mmmmhhh you shouldn't be arrived here...something bad!"),
    }

    selectedcurrency
}

pub fn load_fiat_values() -> FiatList {
    let fiat_names = r#"
    {
      "AED": {
        "symbol": "AED",
        "name": "United Arab Emirates Dirham",
        "symbol_native": "د.إ.‏",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "AED",
        "emoji": "🇦🇪",
        "name_plural": "UAE dirhams",
        "price": true
      },
      "AFN": {
        "symbol": "Af",
        "name": "Afghan Afghani",
        "symbol_native": "؋",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "AFN",
        "emoji": "",
        "name_plural": "Afghan Afghanis"
      },
      "ALL": {
        "symbol": "ALL",
        "name": "Albanian Lek",
        "symbol_native": "Lek",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "ALL",
        "emoji": "",
        "name_plural": "Albanian lekë"
      },
      "AMD": {
        "symbol": "AMD",
        "name": "Armenian Dram",
        "symbol_native": "դր.",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "AMD",
        "emoji": "",
        "name_plural": "Armenian drams"
      },
      "ANG": {
        "symbol": "ANG",
        "name": "Netherlands Antillean Guilder",
        "symbol_native": "NAƒ",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "ANG",
        "emoji": "🇧🇶",
        "name_plural": "ANG",
        "price": true
      },
      "AOA": {
        "symbol": "AOA",
        "name": "Angolan Kwanza",
        "symbol_native": "Kz",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "AOA",
        "emoji": "🇦🇴",
        "name_plural": "AOA",
        "price": true
      },
      "ARS": {
        "symbol": "AR$",
        "name": "Peso argentino",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "ARS",
        "emoji": "🇦🇷",
        "name_plural": "Pesos",
        "price": true,
        "locale": "es-AR"
      },
      "AUD": {
        "symbol": "AU$",
        "name": "Australian Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "AUD",
        "emoji": "🇦🇺",
        "name_plural": "Australian dollars",
        "price": true,
        "locale": "en-AU"
      },
      "AZN": {
        "symbol": "man.",
        "name": "Azerbaijani Manat",
        "symbol_native": "ман.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "AZN",
        "emoji": "🇦🇿",
        "name_plural": "Azerbaijani manats",
        "price": true
      },
      "BAM": {
        "symbol": "KM",
        "name": "Bosnia-Herzegovina Convertible Mark",
        "symbol_native": "KM",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BAM",
        "emoji": "",
        "name_plural": "Bosnia-Herzegovina convertible marks"
      },
      "BDT": {
        "symbol": "Tk",
        "name": "Bangladeshi Taka",
        "symbol_native": "৳",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BDT",
        "emoji": "🇧🇩",
        "name_plural": "Bangladeshi takas",
        "price": true
      },
      "BGN": {
        "symbol": "BGN",
        "name": "Bulgarian Lev",
        "symbol_native": "лв.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BGN",
        "emoji": "",
        "name_plural": "Bulgarian leva"
      },
      "BHD": {
        "symbol": "BHD",
        "name": "Bahraini Dinar",
        "symbol_native": "د.ب.‏",
        "decimal_digits": 3,
        "rounding": 0,
        "code": "BHD",
        "emoji": "🇧🇭",
        "name_plural": "Bahraini dinars",
        "price": true
      },
      "BIF": {
        "symbol": "FBu",
        "name": "Burundian Franc",
        "symbol_native": "FBu",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "BIF",
        "emoji": "",
        "name_plural": "Burundian francs"
      },
      "BMD": {
        "symbol": "BMD",
        "name": "Bermudan Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BMD",
        "emoji": "🇧🇲",
        "name_plural": "Bermudan Dollar",
        "price": true
      },
      "BND": {
        "symbol": "BN$",
        "name": "Brunei Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BND",
        "emoji": "",
        "name_plural": "Brunei dollars"
      },
      "BOB": {
        "symbol": "Bs",
        "name": "Boliviano",
        "symbol_native": "Bs",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BOB",
        "emoji": "🇧🇴",
        "name_plural": "Bolivianos",
        "price": true,
        "locale": "es-BO"
      },
      "BRL": {
        "symbol": "R$",
        "name": "Brazilian Real",
        "symbol_native": "R$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BRL",
        "emoji": "🇧🇷",
        "name_plural": "Brazilian reals",
        "price": true,
        "locale": "pt-BR"
      },
      "BWP": {
        "symbol": "BWP",
        "name": "Botswanan Pula",
        "symbol_native": "P",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BWP",
        "emoji": "",
        "name_plural": "Botswanan pulas",
        "price": true
      },
      "BYN": {
        "symbol": "Br",
        "name": "Belarusian Ruble",
        "symbol_native": "руб.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BYN",
        "emoji": "🇧🇾",
        "name_plural": "Belarusian rubles",
        "price": true
      },
      "BZD": {
        "symbol": "BZ$",
        "name": "Belize Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "BZD",
        "emoji": "",
        "name_plural": "Belize dollars"
      },
      "CAD": {
        "symbol": "CA$",
        "name": "Canadian Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "CAD",
        "emoji": "🇨🇦",
        "name_plural": "Canadian dollars",
        "price": true,
        "locale": "en-CA"
      },
      "CDF": {
        "symbol": "CDF",
        "name": "Congolese Franc",
        "symbol_native": "FrCD",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "CDF",
        "emoji": "",
        "name_plural": "Congolese francs",
        "price": true
      },
      "CHF": {
        "symbol": "CHF",
        "name": "Swiss Franc",
        "symbol_native": "CHF",
        "decimal_digits": 2,
        "rounding": 0.05,
        "code": "CHF",
        "emoji": "🇨🇭",
        "name_plural": "Swiss francs",
        "price": true
      },
      "CLP": {
        "symbol": "CL$",
        "name": "Peso chileno",
        "symbol_native": "$",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "CLP",
        "emoji": "🇨🇱",
        "name_plural": "Pesos",
        "price": true,
        "locale": "es-CL"
      },
      "CNY": {
        "symbol": "CN¥",
        "name": "Chinese Yuan",
        "symbol_native": "CN¥",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "CNY",
        "emoji": "🇨🇳",
        "name_plural": "Chinese yuan",
        "price": true
      },
      "COP": {
        "symbol": "CO$",
        "name": "Peso colombiano",
        "symbol_native": "$",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "COP",
        "emoji": "🇨🇴",
        "name_plural": "Pesos",
        "price": true,
        "locale": "es-CO"
      },
      "CRC": {
        "symbol": "₡",
        "name": "Colón",
        "symbol_native": "₡",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "CRC",
        "emoji": "🇨🇷",
        "name_plural": "Colones",
        "price": true,
        "locale": "es-CR"
      },
      "CUP": {
        "symbol": "CU$",
        "name": "Peso cubano",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "CUP",
        "emoji": "🇨🇺",
        "name_plural": "Pesos",
        "price": true,
        "locale": "es-AR"
      },
      "CVE": {
        "symbol": "CV$",
        "name": "Cape Verdean Escudo",
        "symbol_native": "CV$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "CVE",
        "emoji": "",
        "name_plural": "Cape Verdean escudos"
      },
      "CZK": {
        "symbol": "Kč",
        "name": "Czech Republic Koruna",
        "symbol_native": "Kč",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "CZK",
        "emoji": "🇨🇿",
        "name_plural": "Czech Republic korunas",
        "price": true
      },
      "DJF": {
        "symbol": "Fdj",
        "name": "Djiboutian Franc",
        "symbol_native": "Fdj",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "DJF",
        "emoji": "",
        "name_plural": "Djiboutian francs"
      },
      "DKK": {
        "symbol": "Dkr",
        "name": "Danish Krone",
        "symbol_native": "kr",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "DKK",
        "emoji": "🇩🇰",
        "name_plural": "Danish kroner",
        "price": true
      },
      "DOP": {
        "symbol": "RD$",
        "name": "Peso dominicano",
        "symbol_native": "RD$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "DOP",
        "emoji": "🇩🇴",
        "name_plural": "Pesos",
        "price": true,
        "locale": "es-DO"
      },
      "DZD": {
        "symbol": "DA",
        "name": "Algerian Dinar",
        "symbol_native": "د.ج.‏",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "DZD",
        "emoji": "🇩🇿",
        "name_plural": "Algerian dinars",
        "price": true
      },
      "EEK": {
        "symbol": "Ekr",
        "name": "Estonian Kroon",
        "symbol_native": "kr",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "EEK",
        "emoji": "",
        "name_plural": "Estonian kroons"
      },
      "EGP": {
        "symbol": "EGP",
        "name": "Egyptian Pound",
        "symbol_native": "ج.م.‏",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "EGP",
        "emoji": "🇪🇬",
        "name_plural": "Egyptian pounds",
        "price": true
      },
      "ERN": {
        "symbol": "Nfk",
        "name": "Eritrean Nakfa",
        "symbol_native": "Nfk",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "ERN",
        "emoji": "",
        "name_plural": "Eritrean nakfas"
      },
      "ETB": {
        "symbol": "Br",
        "name": "Ethiopian Birr",
        "symbol_native": "Br",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "ETB",
        "emoji": "🇪🇹",
        "name_plural": "Ethiopian birrs",
        "price": true
      },
      "EUR": {
        "symbol": "€",
        "name": "Euro",
        "symbol_native": "€",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "EUR",
        "emoji": "🇪🇺",
        "name_plural": "euros",
        "price": true,
        "locale": "de-DE"
      },
      "GBP": {
        "symbol": "£",
        "name": "British Pound Sterling",
        "symbol_native": "£",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "GBP",
        "emoji": "🇬🇧",
        "name_plural": "British pounds sterling",
        "price": true
      },
      "GEL": {
        "symbol": "GEL",
        "name": "Georgian Lari",
        "symbol_native": "GEL",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "GEL",
        "emoji": "🇬🇪",
        "name_plural": "Georgian laris",
        "price": true
      },
      "GHS": {
        "symbol": "GH₵",
        "name": "Ghanaian Cedi",
        "symbol_native": "GH₵",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "GHS",
        "emoji": "🇬🇭",
        "name_plural": "Ghanaian cedis",
        "price": true
      },
      "GNF": {
        "symbol": "FG",
        "name": "Guinean Franc",
        "symbol_native": "FG",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "GNF",
        "emoji": "",
        "name_plural": "Guinean francs"
      },
      "GTQ": {
        "symbol": "GTQ",
        "name": "Quetzal",
        "symbol_native": "Q",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "GTQ",
        "emoji": "🇬🇹",
        "name_plural": "Quetzales",
        "price": true
      },
      "HKD": {
        "symbol": "HK$",
        "name": "Hong Kong Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "HKD",
        "emoji": "🇭🇰",
        "name_plural": "Hong Kong dollars",
        "price": true
      },
      "HNL": {
        "symbol": "HNL",
        "name": "Lempira",
        "symbol_native": "L",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "HNL",
        "emoji": "🇭🇳",
        "name_plural": "Lempiras",
        "price": true
      },
      "HRK": {
        "symbol": "kn",
        "name": "Croatian Kuna",
        "symbol_native": "kn",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "HRK",
        "emoji": "",
        "name_plural": "Croatian kunas"
      },
      "HUF": {
        "symbol": "Ft",
        "name": "Hungarian Forint",
        "symbol_native": "Ft",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "HUF",
        "emoji": "🇭🇺",
        "name_plural": "Hungarian forints",
        "price": true
      },
      "IDR": {
        "symbol": "Rp",
        "name": "Indonesian Rupiah",
        "symbol_native": "Rp",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "IDR",
        "emoji": "🇮🇩",
        "name_plural": "Indonesian rupiahs",
        "price": true
      },
      "ILS": {
        "symbol": "₪",
        "name": "Israeli New Sheqel",
        "symbol_native": "₪",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "ILS",
        "emoji": "🇮🇱",
        "name_plural": "Israeli new sheqels",
        "price": true
      },
      "INR": {
        "symbol": "Rs",
        "name": "Indian Rupee",
        "symbol_native": "টকা",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "INR",
        "emoji": "🇮🇳",
        "name_plural": "Indian rupees",
        "price": true
      },
      "IQD": {
        "symbol": "IQD",
        "name": "Iraqi Dinar",
        "symbol_native": "د.ع.‏",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "IQD",
        "emoji": "",
        "name_plural": "Iraqi dinars"
      },
      "IRR": {
        "symbol": "IRR",
        "name": "Iranian Rial",
        "symbol_native": "﷼",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "IRR",
        "emoji": "",
        "name_plural": "Iranian rials"
      },
      "ISK": {
        "symbol": "Ikr",
        "name": "Icelandic Króna",
        "symbol_native": "kr",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "ISK",
        "emoji": "",
        "name_plural": "Icelandic krónur"
      },
      "JMD": {
        "symbol": "J$",
        "name": "Jamaican Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "JMD",
        "emoji": "🇯🇲",
        "name_plural": "Jamaican dollars",
        "price": true
      },
      "JOD": {
        "symbol": "JD",
        "name": "Jordanian Dinar",
        "symbol_native": "د.أ.‏",
        "decimal_digits": 3,
        "rounding": 0,
        "code": "JOD",
        "emoji": "🇯🇴",
        "name_plural": "Jordanian dinars",
        "price": true
      },
      "JPY": {
        "symbol": "¥",
        "name": "Japanese Yen",
        "symbol_native": "￥",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "JPY",
        "emoji": "🇯🇵",
        "name_plural": "Japanese yen",
        "price": true
      },
      "KES": {
        "symbol": "Ksh",
        "name": "Kenyan Shilling",
        "symbol_native": "Ksh",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "KES",
        "emoji": "🇰🇪",
        "name_plural": "Kenyan shillings",
        "price": true
      },
      "KGS": {
        "symbol": "KGS",
        "name": "Kyrgystani Som",
        "symbol_native": "KGS",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "KGS",
        "emoji": "🇰🇬",
        "name_plural": "Kyrgystani Som",
        "price": true
      },
      "KHR": {
        "symbol": "KHR",
        "name": "Cambodian Riel",
        "symbol_native": "៛",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "KHR",
        "emoji": "",
        "name_plural": "Cambodian riels"
      },
      "KMF": {
        "symbol": "CF",
        "name": "Comorian Franc",
        "symbol_native": "FC",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "KMF",
        "emoji": "",
        "name_plural": "Comorian francs"
      },
      "KRW": {
        "symbol": "₩",
        "name": "South Korean Won",
        "symbol_native": "₩",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "KRW",
        "emoji": "🇰🇷",
        "name_plural": "South Korean won",
        "price": true
      },
      "KWD": {
        "symbol": "KD",
        "name": "Kuwaiti Dinar",
        "symbol_native": "د.ك.‏",
        "decimal_digits": 3,
        "rounding": 0,
        "code": "KWD",
        "emoji": "",
        "name_plural": "Kuwaiti dinars"
      },
      "KZT": {
        "symbol": "KZT",
        "name": "Kazakhstani Tenge",
        "symbol_native": "тңг.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "KZT",
        "emoji": "🇰🇿",
        "name_plural": "Kazakhstani tenges",
        "price": true
      },
      "LBP": {
        "symbol": "L.L.",
        "name": "Lebanese Pound",
        "symbol_native": "ل.ل.‏",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "LBP",
        "emoji": "🇱🇧",
        "name_plural": "Lebanese pounds",
        "price": true
      },
      "LKR": {
        "symbol": "SLRs",
        "name": "Sri Lankan Rupee",
        "symbol_native": "SL Re",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "LKR",
        "emoji": "🇱🇰",
        "name_plural": "Sri Lankan rupees",
        "price": true
      },
      "LTL": {
        "symbol": "Lt",
        "name": "Lithuanian Litas",
        "symbol_native": "Lt",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "LTL",
        "emoji": "",
        "name_plural": "Lithuanian litai"
      },
      "LVL": {
        "symbol": "Ls",
        "name": "Latvian Lats",
        "symbol_native": "Ls",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "LVL",
        "emoji": "",
        "name_plural": "Latvian lati"
      },
      "LYD": {
        "symbol": "LD",
        "name": "Libyan Dinar",
        "symbol_native": "د.ل.‏",
        "decimal_digits": 3,
        "rounding": 0,
        "code": "LYD",
        "emoji": "",
        "name_plural": "Libyan dinars"
      },
      "MAD": {
        "symbol": "MAD",
        "name": "Moroccan Dirham",
        "symbol_native": "د.م.‏",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MAD",
        "emoji": "🇲🇦",
        "name_plural": "Moroccan dirhams",
        "price": true
      },
      "MDL": {
        "symbol": "MDL",
        "name": "Moldovan Leu",
        "symbol_native": "MDL",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MDL",
        "emoji": "",
        "name_plural": "Moldovan lei"
      },
      "MGA": {
        "symbol": "MGA",
        "name": "Malagasy Ariary",
        "symbol_native": "MGA",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "MGA",
        "emoji": "",
        "name_plural": "Malagasy Ariaries"
      },
      "MKD": {
        "symbol": "MKD",
        "name": "Macedonian Denar",
        "symbol_native": "MKD",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MKD",
        "emoji": "",
        "name_plural": "Macedonian denari"
      },
      "MLC": {
        "symbol": "MLC",
        "name": "Moneda Libremente Convertible",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MLC",
        "emoji": "🇨🇺",
        "name_plural": "MLC",
        "price": true,
        "locale": "es-AR"
      },
      "MMK": {
        "symbol": "MMK",
        "name": "Myanma Kyat",
        "symbol_native": "K",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "MMK",
        "emoji": "",
        "name_plural": "Myanma kyats"
      },
      "MOP": {
        "symbol": "MOP$",
        "name": "Macanese Pataca",
        "symbol_native": "MOP$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MOP",
        "emoji": "",
        "name_plural": "Macanese patacas"
      },
      "MUR": {
        "symbol": "MURs",
        "name": "Mauritian Rupee",
        "symbol_native": "MURs",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "MUR",
        "emoji": "",
        "name_plural": "Mauritian rupees"
      },
      "MXN": {
        "symbol": "MX$",
        "name": "Peso mexicano",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MXN",
        "emoji": "🇲🇽",
        "name_plural": "Pesos",
        "price": true,
        "locale": "es-MX"
      },
      "MYR": {
        "symbol": "RM",
        "name": "Malaysian Ringgit",
        "symbol_native": "RM",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MYR",
        "emoji": "🇲🇾",
        "name_plural": "Malaysian ringgits",
        "price": true
      },
      "MZN": {
        "symbol": "MTn",
        "name": "Mozambican Metical",
        "symbol_native": "MTn",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "MZN",
        "emoji": "",
        "name_plural": "Mozambican meticals"
      },
      "NAD": {
        "symbol": "N$",
        "name": "Namibian Dollar",
        "symbol_native": "N$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "NAD",
        "emoji": "🇳🇦",
        "name_plural": "Namibian dollars",
        "price": true
      },
      "NGN": {
        "symbol": "₦",
        "name": "Nigerian Naira",
        "symbol_native": "₦",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "NGN",
        "emoji": "🇳🇬",
        "name_plural": "Nigerian nairas",
        "price": true
      },
      "NIO": {
        "symbol": "C$",
        "name": "Nicaraguan Córdoba",
        "symbol_native": "C$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "NIO",
        "emoji": "🇳🇮",
        "name_plural": "Nicaraguan córdobas",
        "price": true
      },
      "NOK": {
        "symbol": "Nkr",
        "name": "Norwegian Krone",
        "symbol_native": "kr",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "NOK",
        "emoji": "🇳🇴",
        "name_plural": "Norwegian kroner",
        "price": true
      },
      "NPR": {
        "symbol": "NPRs",
        "name": "Nepalese Rupee",
        "symbol_native": "नेरू",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "NPR",
        "emoji": "🇳🇵",
        "name_plural": "Nepalese rupees",
        "price": true
      },
      "NZD": {
        "symbol": "NZ$",
        "name": "New Zealand Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "NZD",
        "emoji": "🇳🇿",
        "name_plural": "New Zealand dollars",
        "price": true
      },
      "OMR": {
        "symbol": "OMR",
        "name": "Omani Rial",
        "symbol_native": "ر.ع.‏",
        "decimal_digits": 3,
        "rounding": 0,
        "code": "OMR",
        "emoji": "",
        "name_plural": "Omani rials"
      },
      "PAB": {
        "symbol": "B/.",
        "name": "Panamanian Balboa",
        "symbol_native": "B/.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "PAB",
        "emoji": "🇵🇦",
        "name_plural": "Balboas",
        "price": true
      },
      "PEN": {
        "symbol": "S/.",
        "name": "Peruvian Nuevo Sol",
        "symbol_native": "S/.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "PEN",
        "emoji": "🇵🇪",
        "name_plural": "Nuevos soles peruanos",
        "price": true,
        "locale": "es-PE"
      },
      "PHP": {
        "symbol": "₱",
        "name": "Philippine Peso",
        "symbol_native": "₱",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "PHP",
        "emoji": "🇵🇭",
        "name_plural": "Pesos",
        "price": true
      },
      "PKR": {
        "symbol": "PKRs",
        "name": "Pakistani Rupee",
        "symbol_native": "₨",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "PKR",
        "emoji": "🇵🇰",
        "name_plural": "Pakistani rupees",
        "price": true
      },
      "PLN": {
        "symbol": "zł",
        "name": "Polish Zloty",
        "symbol_native": "zł",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "PLN",
        "emoji": "🇵🇱",
        "name_plural": "Polish zlotys",
        "price": true
      },
      "PYG": {
        "symbol": "₲",
        "name": "Paraguayan Guarani",
        "symbol_native": "₲",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "PYG",
        "emoji": "🇵🇾",
        "name_plural": "Guaranis",
        "price": true
      },
      "QAR": {
        "symbol": "QR",
        "name": "Qatari Rial",
        "symbol_native": "ر.ق.‏",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "QAR",
        "emoji": "🇶🇦",
        "name_plural": "Qatari rials",
        "price": true
      },
      "RON": {
        "symbol": "RON",
        "name": "Romanian Leu",
        "symbol_native": "RON",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "RON",
        "emoji": "🇷🇴",
        "name_plural": "Romanian lei",
        "price": true
      },
      "RSD": {
        "symbol": "din.",
        "name": "Serbian Dinar",
        "symbol_native": "дин.",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "RSD",
        "emoji": "",
        "name_plural": "Serbian dinars",
        "price": true
      },
      "RUB": {
        "symbol": "RUB",
        "name": "руб",
        "symbol_native": "₽.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "RUB",
        "emoji": "🇷🇺",
        "name_plural": "руб",
        "price": true
      },
      "RWF": {
        "symbol": "RWF",
        "name": "Rwandan Franc",
        "symbol_native": "FR",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "RWF",
        "emoji": "",
        "name_plural": "Rwandan francs"
      },
      "SAR": {
        "symbol": "SR",
        "name": "Saudi Riyal",
        "symbol_native": "ر.س.‏",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "SAR",
        "emoji": "🇸🇦",
        "name_plural": "Saudi riyals",
        "price": true
      },
      "SDG": {
        "symbol": "SDG",
        "name": "Sudanese Pound",
        "symbol_native": "SDG",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "SDG",
        "emoji": "",
        "name_plural": "Sudanese pounds"
      },
      "SEK": {
        "symbol": "Skr",
        "name": "Swedish Krona",
        "symbol_native": "kr",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "SEK",
        "emoji": "🇸🇪",
        "name_plural": "Swedish kronor",
        "price": true
      },
      "SGD": {
        "symbol": "S$",
        "name": "Singapore Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "SGD",
        "emoji": "🇸🇬",
        "name_plural": "Singapore dollars",
        "price": true
      },
      "SOS": {
        "symbol": "Ssh",
        "name": "Somali Shilling",
        "symbol_native": "Ssh",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "SOS",
        "emoji": "",
        "name_plural": "Somali shillings"
      },
      "SYP": {
        "symbol": "SY£",
        "name": "Syrian Pound",
        "symbol_native": "ل.س.‏",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "SYP",
        "emoji": "",
        "name_plural": "Syrian pounds"
      },
      "THB": {
        "symbol": "฿",
        "name": "Thai Baht",
        "symbol_native": "฿",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "THB",
        "emoji": "🇹🇭",
        "name_plural": "Thai baht",
        "price": true
      },
      "TND": {
        "symbol": "DT",
        "name": "Tunisian Dinar",
        "symbol_native": "د.ت.‏",
        "decimal_digits": 3,
        "rounding": 0,
        "code": "TND",
        "emoji": "🇹🇳",
        "name_plural": "Tunisian dinars",
        "price": true
      },
      "TOP": {
        "symbol": "T$",
        "name": "Tongan Paʻanga",
        "symbol_native": "T$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "TOP",
        "emoji": "",
        "name_plural": "Tongan paʻanga"
      },
      "TRY": {
        "symbol": "TL",
        "name": "Turkish Lira",
        "symbol_native": "TL",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "TRY",
        "emoji": "🇹🇷",
        "name_plural": "Turkish Lira",
        "price": true
      },
      "TTD": {
        "symbol": "TT$",
        "name": "Trinidad and Tobago Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "TTD",
        "emoji": "🇹🇹",
        "name_plural": "Trinidad and Tobago dollars",
        "price": true
      },
      "TWD": {
        "symbol": "NT$",
        "name": "New Taiwan Dollar",
        "symbol_native": "NT$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "TWD",
        "emoji": "🇹🇼",
        "name_plural": "New Taiwan dollars",
        "price": true
      },
      "TZS": {
        "symbol": "TSh",
        "name": "Tanzanian Shilling",
        "symbol_native": "TSh",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "TZS",
        "emoji": "🇹🇿",
        "name_plural": "Tanzanian shillings",
        "price": true
      },
      "UAH": {
        "symbol": "₴",
        "name": "Ukrainian Hryvnia",
        "symbol_native": "₴",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "UAH",
        "emoji": "🇺🇦",
        "name_plural": "Ukrainian hryvnias",
        "price": true
      },
      "UGX": {
        "symbol": "USh",
        "name": "Ugandan Shilling",
        "symbol_native": "USh",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "UGX",
        "emoji": "🇺🇬",
        "name_plural": "Ugandan shillings",
        "price": true
      },
      "USD": {
        "symbol": "$",
        "name": "US Dollar",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "USD",
        "emoji": "🇺🇸",
        "name_plural": "US dollars",
        "price": true,
        "locale": "en-US"
      },
      "USDSV": {
        "symbol": "$",
        "name": "USD en El Salvador",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "USDSV",
        "emoji": "🇺🇸🇸🇻",
        "name_plural": "USD en El Salvador",
        "price": true,
        "locale": "es-SV"
      },
      "USDVE": {
        "symbol": "$",
        "name": "USD en Bs",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "USDVE",
        "emoji": "🇺🇸🇻🇪",
        "name_plural": "USD en Bs",
        "price": true,
        "locale": "es-VE"
      },
      "USDUY": {
        "symbol": "$",
        "name": "USD en Uruguay",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "USDUY",
        "emoji": "🇺🇸🇺🇾",
        "name_plural": "USD en Uruguay",
        "price": true,
        "locale": "es-UY"
      },
      "UYU": {
        "symbol": "$U",
        "name": "Peso uruguayo",
        "symbol_native": "$",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "UYU",
        "emoji": "🇺🇾",
        "name_plural": "Pesos",
        "price": true,
        "locale": "es-UY"
      },
      "UZS": {
        "symbol": "UZS",
        "name": "Uzbekistan Som",
        "symbol_native": "UZS",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "UZS",
        "emoji": "🇺🇿",
        "name_plural": "Uzbekistan som",
        "price": true
      },
      "VES": {
        "symbol": "Bs.",
        "name": "Bolívar",
        "symbol_native": "Bs.",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "VES",
        "emoji": "🇻🇪",
        "name_plural": "Bolívares",
        "price": true,
        "locale": "es-VE"
      },
      "VND": {
        "symbol": "₫",
        "name": "Vietnamese Dong",
        "symbol_native": "₫",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "VND",
        "emoji": "🇻🇳",
        "name_plural": "Vietnamese dong",
        "price": true
      },
      "XAF": {
        "symbol": "FCFA",
        "name": "CFA Franc BEAC",
        "symbol_native": "FCFA",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "XAF",
        "emoji": "🏳️",
        "name_plural": "CFA francs BEAC",
        "price": true
      },
      "XOF": {
        "symbol": "CFA",
        "name": "CFA Franc BCEAO",
        "symbol_native": "CFA",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "XOF",
        "emoji": "🏳️",
        "name_plural": "CFA francs BCEAO",
        "price": true
      },
      "YER": {
        "symbol": "YR",
        "name": "Yemeni Rial",
        "symbol_native": "ر.ي.‏",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "YER",
        "emoji": "",
        "name_plural": "Yemeni rials"
      },
      "ZAR": {
        "symbol": "R",
        "name": "South African Rand",
        "symbol_native": "R",
        "decimal_digits": 2,
        "rounding": 0,
        "code": "ZAR",
        "emoji": "🇿🇦",
        "name_plural": "South African rand",
        "price": true
      },
      "ZMK": {
        "symbol": "ZK",
        "name": "Zambian Kwacha",
        "symbol_native": "ZK",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "ZMK",
        "emoji": "",
        "name_plural": "Zambian kwachas"
      },
      "ZWL": {
        "symbol": "ZWL$",
        "name": "Zimbabwean Dollar",
        "symbol_native": "ZWL$",
        "decimal_digits": 0,
        "rounding": 0,
        "code": "ZWL",
        "emoji": "🇿🇼",
        "name_plural": "Zimbabwean Dollar"
      }
    }"#;

    let fiat_json: FiatNames = serde_json::from_str(fiat_names).unwrap();

    let mut fiatlist = FiatList::new();

    for elem in fiat_json.iter() {
        fiatlist.push((elem.0.to_string(), elem.1.name.clone()));
    }

    //Return list
    fiatlist.sort_by(|a, b| a.0.cmp(&b.0));

    fiatlist
}
