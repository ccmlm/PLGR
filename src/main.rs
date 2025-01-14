use clap::Parser;
use ruc::*;
use secp256k1::SecretKey;
use std::{collections::BTreeMap, fs, str::FromStr};
use tokio::runtime::Runtime;
use web3::{
    contract::{Contract, Options},
    signing::{Key, SecretKeyRef},
    transports::Http,
    types::{Address, U256},
    Web3,
};

const BSC_MAINNET: &str = "https://bsc-dataseed3.binance.org";
const CONTRACT_MAINNET: &str = "0x6aa91cbfe045f9d154050226fcc830ddba886ced";

const BSC_TESTNET: &str = "https://data-seed-prebsc-1-s1.binance.org:8545";
const CONTRACT_TESTNET: &str = "0xffe5548b5c3023b3277c1a6f24ac6382a0087db5";

const GOOD: &str = "\x1b[35;01mGOOD\x1b[0m";
const FAIL: &str = "\x1b[31;01mFAIL\x1b[0m";
// const UNKNOWN: &str = "\x1b[39;01mUNKNOWN\x1b[0m";

fn main() {
    pnk!(run());
}

fn run() -> Result<()> {
    let rt = Runtime::new().c(d!())?;
    let args = Args::parse();

    let url = args
        .rpc_addr
        .as_deref()
        .unwrap_or(alt!(args.bsc_testnet, BSC_TESTNET, BSC_MAINNET));
    let contract_addr = args.contract.as_deref().unwrap_or(alt!(
        args.bsc_testnet,
        CONTRACT_TESTNET,
        CONTRACT_MAINNET
    ));

    let transport = Http::new(url).c(d!())?;
    let web3 = Web3::new(transport);

    let prvk = fs::read_to_string(args.privkey_path)
        .c(d!())
        .and_then(|c| SecretKey::from_str(c.trim()).c(d!()))?;
    let contract = Contract::from_json(
        web3.eth(),
        Address::from_str(contract_addr).c(d!())?,
        include_bytes!("token.json"),
    )
    .c(d!())?;

    let contents = fs::read_to_string(args.entries_path).c(d!())?;

    let mut entries = vec![];
    for l in contents.lines() {
        let line = l.replace(' ', "");
        alt!(line.is_empty(), continue);
        let en = line.split(',').collect::<Vec<_>>();
        if 2 != en.len() {
            return Err(eg!(format!("Invalid entry: {}", l)));
        }
        if !en[0].starts_with("0x") || 42 != en[0].len() {
            return Err(eg!(format!("Invalid entry: {}", l)));
        }
        let receiver = Address::from_str(en[0]).c(d!(format!("Invalid address: {}", l)))?;
        let amount = en[1]
            .parse::<f64>()
            .or_else(|_| en[1].parse::<u128>().map(|am| am as f64))
            .c(d!(format!("Invalid amount: {}", l)))?;
        entries.push((receiver, amount));
    }

    for (idx, batch) in entries.chunks(50).enumerate() {
        println!("Chunk index(start from 0): {}", idx);
        run_batch(&web3, &rt, batch, prvk, &contract).c(d!())?;
    }

    Ok(())
}

fn run_batch(
    web3: &Web3<Http>,
    rt: &Runtime,
    entries: &[(Address, f64)],
    prvk: SecretKey,
    contract: &Contract<Http>,
) -> Result<()> {
    let entries_pre_balances = get_balances(rt, entries, contract).c(d!())?;

    let sender = SecretKeyRef::new(&prvk).address();
    let total_am = (entries
        .iter()
        .fold(entries.len() as f64, |acc, i| acc + i.1)
        * (10u128.pow(18) as f64)) as u128;
    let balance: U256 = rt
        .block_on(contract.query("balanceOf", (sender,), None, Options::default(), None))
        .c(d!())?;
    let balance = balance.as_u128();
    if total_am > balance {
        let mint_am = (total_am - balance) * 100;
        println!("=> Minting: {}", to_float_str(mint_am));
        rt.block_on(contract.signed_call(
            "mint",
            (mint_am,),
            Options::default(),
            SecretKeyRef::new(&prvk),
        ))
        .c(d!("Insufficient balance, and mint failed!"))?;
        sleep_ms!(5000);
        let new_balance: U256 = rt
            .block_on(contract.query("balanceOf", (sender,), None, Options::default(), None))
            .c(d!())?;
        if new_balance.as_u128() - balance != mint_am {
            return Err(eg!("Insufficient balance, and mint failed!"));
        }
    }

    let nonce = rt
        .block_on(web3.eth().transaction_count(sender, None))
        .c(d!("Fail to get nonce"))?
        .as_u128();

    println!("=> \x1b[37;1mSending from: 0x{:x}\x1b[0m", sender);
    for (idx, (receiver, amount)) in entries.iter().copied().enumerate() {
        rt.block_on(async {
            let am = (amount * (10u128.pow(18) as f64)) as u128;
            let options = Options {
                nonce: Some((idx as u128 + nonce).into()),
                ..Default::default()
            };
            let transaction_hash = pnk!(
                contract
                    .signed_call(
                        "transfer",
                        (receiver, am),
                        options,
                        SecretKeyRef::new(&prvk),
                    )
                    .await
            );
            println!(
                "=> [ Entry-{} ], Amount: {}, SendTo: 0x{:x}, TxHash: {}",
                idx,
                to_float_str(am),
                receiver,
                transaction_hash,
            );
        });
    }

    println!("=> \x1b[37;1mSleep 10 seconds, and check on-chain results...\x1b[0m");
    sleep_ms!(10_000);

    let mut fail_cnter = 0;

    let ets = entries
        .iter()
        .copied()
        .fold(BTreeMap::new(), |mut acc, (k, v)| {
            *acc.entry(k).or_insert(0.0) += v;
            acc
        });
    for (idx, ((receiver, amount), pre_balance)) in ets
        .into_iter()
        .zip(entries_pre_balances.into_iter().map(|(_, v)| v))
        .enumerate()
    {
        let am = (amount * (10u128.pow(18) as f64)) as u128;
        let mut cnter = 2;
        let (status, balance) = loop {
            let balance: U256 = rt
                .block_on(contract.query("balanceOf", (receiver,), None, Options::default(), None))
                .c(d!())
                .or_else(|e| {
                    sleep_ms!(200);
                    rt.block_on(contract.query(
                        "balanceOf",
                        (receiver,),
                        None,
                        Options::default(),
                        None,
                    ))
                    .c(d!(e))
                })?;
            let balance = balance.as_u128();
            if am / 10u128.pow(15) == (balance - pre_balance) / 10u128.pow(15) {
                break (GOOD, balance);
            } else if 0 == cnter {
                fail_cnter += 1;
                break (FAIL, balance);
            } else {
                cnter -= 1;
                sleep_ms!(3000);
            }
        };
        println!(
                "=> Result-{}: {}, Amount: {}, BalanceDiff: {}, NewBalance: {}, OldBalance: {}, Receiver: 0x{:x}",
                idx,
                status,
                amount,
                to_float_str(balance - pre_balance),
                to_float_str(balance),
                to_float_str(pre_balance),
                receiver,
            );
    }

    if 0 < fail_cnter {
        Err(eg!("!! {} entries failed !!", fail_cnter))
    } else {
        Ok(())
    }
}

fn get_balances(
    rt: &Runtime,
    entries: &[(Address, f64)],
    contract: &Contract<Http>,
) -> Result<BTreeMap<Address, u128>> {
    let mut balances = BTreeMap::new();

    for (idx, en) in entries.iter().enumerate() {
        let receiver = en.0;
        let data = (receiver,);
        let balance: U256 = rt
            .block_on(contract.query("balanceOf", data, None, Options::default(), None))
            .c(d!())
            .or_else(|e| {
                sleep_ms!(200);
                rt.block_on(contract.query("balanceOf", data, None, Options::default(), None))
                    .c(d!(e))
            })?;
        println!(
            "Got balance nth: {}, addr: 0x{:x}, amount: {}",
            idx, receiver, balance
        );
        balances.insert(receiver, balance.as_u128());
    }

    Ok(balances)
}

fn to_float_str(n: u128) -> String {
    let base = 10u128.pow(18);
    let i = n / base;
    let j = n - i * base;

    let pads = if 0 == i {
        18 - (1..=18)
            .into_iter()
            .find(|&k| 0 == j / 10u128.pow(k))
            .unwrap()
    } else {
        0
    };
    let pads = (0..pads).map(|_| '0').collect::<String>();

    (i.to_string() + "." + &pads + j.to_string().trim_end_matches('0'))
        .trim_end_matches('.')
        .to_owned()
}

#[derive(Parser, Debug)]
#[clap(about, version, author)]
struct Args {
    #[clap(long, help = "Optional, default to BSC mainnet")]
    bsc_testnet: bool,
    #[clap(
        short = 'p',
        long,
        help = "A file containing who and how much to transfer"
    )]
    entries_path: String,
    #[clap(short = 'K', long, help = "A file containing your private key")]
    privkey_path: String,
    #[clap(short = 'a', long, help = "Optional, like: http://***:8545")]
    rpc_addr: Option<String>,
    #[clap(short = 'c', long, help = "Optional, like: 0x816d8...40C9a")]
    contract: Option<String>,
}
