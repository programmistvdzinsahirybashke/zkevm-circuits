use super::{
    parse,
    spec::{AccountMatch, Env, StateTest, DEFAULT_BASE_FEE},
};
use crate::{utils::MainnetFork, Compiler};
use anyhow::{anyhow, bail, Context, Result};
use eth_types::{geth_types::Account, Address, Bytes, H256, U256};
use ethers_core::{k256::ecdsa::SigningKey, utils::secret_key_to_address};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryInto,
    str::FromStr,
};
use yaml_rust::Yaml;

#[derive(Debug, Clone)]
enum Ref {
    Any,
    Index(usize),
    Label(String),
}

struct Refs(Vec<Ref>);

impl Refs {
    fn contains_index(&self, idx: usize) -> bool {
        self.0.iter().any(|r| match r {
            Ref::Index(i) => i == &idx,
            Ref::Label(_) => false,
            Ref::Any => true,
        })
    }
    fn contains_label(&self, lbl: &str) -> bool {
        self.0.iter().any(|r| match r {
            Ref::Index(_) => false,
            Ref::Label(l) => l == lbl,
            Ref::Any => true,
        })
    }
}

pub struct YamlStateTestBuilder<'a> {
    compiler: &'a Compiler,
}

impl<'a> YamlStateTestBuilder<'a> {
    pub fn new(compiler: &'a Compiler) -> Self {
        Self { compiler }
    }

    /// generates `StateTest` vectors from a ethereum yaml test specification
    pub fn load_yaml(&mut self, path: &str, source: &str) -> Result<Vec<StateTest>> {
        // get the yaml root element
        let doc = yaml_rust::YamlLoader::load_from_str(source)?
            .into_iter()
            .next()
            .context("get yaml doc")?;

        // collect test names, that are the top-level items in the yaml doc
        let test_names: Vec<_> = doc
            .as_hash()
            .context("parse_hash")?
            .keys()
            .map(|v| v.as_str().context("test_names_as_str"))
            .collect::<Result<_>>()?;

        // for each test defined in the yaml, create the according defined tests
        let mut tests = Vec::new();
        for test_name in test_names {
            let yaml_test = &doc[test_name];

            // parse env
            let env = Self::parse_env(&yaml_test["env"])?;

            // parse pre (account states before executing the transaction)
            let pre: BTreeMap<Address, Account> = self
                .parse_accounts(&yaml_test["pre"], None)?
                .into_iter()
                .map(|(addr, account)| (addr, account.try_into().expect("unable to parse account")))
                .collect();

            // parse transaction
            let yaml_transaction = &yaml_test["transaction"];
            let data_s: Vec<_> = yaml_transaction["data"]
                .as_vec()
                .context("as_vec")?
                .iter()
                .map(|item| self.parse_calldata(item))
                .collect::<Result<_>>()?;

            let gas_limit_s: Vec<_> = yaml_transaction["gasLimit"]
                .as_vec()
                .context("as_vec")?
                .iter()
                .map(Self::parse_u64)
                .collect::<Result<_>>()?;

            let value_s: Vec<_> = yaml_transaction["value"]
                .as_vec()
                .context("as_vec")?
                .iter()
                .map(Self::parse_u256)
                .collect::<Result<_>>()?;

            let max_priority_fee_per_gas =
                Self::parse_u256(&yaml_transaction["maxPriorityFeePerGas"]).ok();
            let max_fee_per_gas = Self::parse_u256(&yaml_transaction["maxFeePerGas"]).ok();

            // Set gas price to `min(max_priority_fee_per_gas + base_fee, max_fee_per_gas)` for
            // EIP-1559 transaction.
            // <https://github.com/ethereum/go-ethereum/blob/1485814f89d8206bb4a1c8e10a4a2893920f683a/core/state_transition.go#L167>
            let gas_price = Self::parse_u256(&yaml_transaction["gasPrice"]).unwrap_or_else(|_| {
                max_fee_per_gas
                    .unwrap()
                    .min(max_priority_fee_per_gas.unwrap() + env.current_base_fee)
            });

            let nonce = Self::parse_u256(&yaml_transaction["nonce"])?;
            let to = Self::parse_to_address(&yaml_transaction["to"])?;
            let secret_key = Self::parse_bytes(&yaml_transaction["secretKey"])?;
            let from = secret_key_to_address(&SigningKey::from_slice(&secret_key)?);

            // parse expects (account states before executing the transaction)
            let mut expects = Vec::new();
            for expect in yaml_test["expect"].as_vec().context("as_vec")?.iter() {
                let networks: Vec<_> = expect["network"]
                    .as_vec()
                    .expect("cannot convert network into vec<string>")
                    .iter()
                    .map(|n| {
                        n.as_str()
                            .expect("cannot convert network into string")
                            .to_string()
                    })
                    .collect();

                let mut exception: bool = false;

                if let Some(exceptions) = expect["expectException"].as_hash() {
                    for (network, _error_type) in exceptions {
                        let network = network.as_str().unwrap().to_string();
                        if MainnetFork::in_network_range(&[network])? {
                            exception = true;
                        }
                    }
                }

                let data_refs = Self::parse_refs(&expect["indexes"]["data"])?;
                let gas_refs = Self::parse_refs(&expect["indexes"]["gas"])?;
                let value_refs = Self::parse_refs(&expect["indexes"]["value"])?;

                // Pass the account addresses before transaction as expected for result.
                let expected_addresses = pre.keys().collect();
                let result = self.parse_accounts(&expect["result"], Some(&expected_addresses))?;

                if MainnetFork::in_network_range(&networks)? {
                    expects.push((exception, data_refs, gas_refs, value_refs, result));
                }
            }

            // generate all the tests defined in the transaction by generating product of
            // data x gas x value
            for (idx_data, calldata) in data_s.iter().enumerate() {
                for (idx_gas, gas_limit) in gas_limit_s.iter().enumerate() {
                    for (idx_value, value) in value_s.iter().enumerate() {
                        // find the first result that fulfills the pattern
                        for (exception, data_refs, gas_refs, value_refs, result) in &expects {
                            // check if this result can be applied to the current test
                            let mut data_label = String::new();
                            if let Some(label) = &calldata.label {
                                if !data_refs.contains_label(label) {
                                    continue;
                                }
                                data_label = format!("({label})");
                            } else if !data_refs.contains_index(idx_data) {
                                continue;
                            }

                            if !gas_refs.contains_index(idx_gas) {
                                continue;
                            }

                            if !value_refs.contains_index(idx_value) {
                                continue;
                            }

                            // add the test
                            tests.push(StateTest {
                                path: path.to_string(),
                                id: format!(
                                    "{test_name}_d{idx_data}{data_label}_g{idx_gas}_v{idx_value}"
                                ),
                                env: env.clone(),
                                pre: pre.clone(),
                                result: result.clone(),
                                from,
                                secret_key: secret_key.clone(),
                                to,
                                gas_limit: *gas_limit,
                                max_priority_fee_per_gas,
                                max_fee_per_gas,
                                gas_price,
                                nonce,
                                value: *value,
                                data: calldata.data.clone(),
                                access_list: calldata.access_list.clone(),
                                exception: *exception,
                            });
                            break;
                        }
                    }
                }
            }
        }

        Ok(tests)
    }

    /// parse env section
    fn parse_env(yaml: &Yaml) -> Result<Env> {
        Ok(Env {
            current_base_fee: Self::parse_u256(&yaml["currentBaseFee"])
                .unwrap_or_else(|_| U256::from(DEFAULT_BASE_FEE)),
            current_coinbase: Self::parse_address(&yaml["currentCoinbase"], None)?,
            current_difficulty: Self::parse_u256(&yaml["currentDifficulty"])?,
            current_gas_limit: Self::parse_u64(&yaml["currentGasLimit"])?,
            current_number: Self::parse_u64(&yaml["currentNumber"])?,
            current_timestamp: Self::parse_u64(&yaml["currentTimestamp"])?,
            previous_hash: Self::parse_hash(&yaml["previousHash"])?,
        })
    }

    /// parse a vector of address=>(storage,balance,code,nonce) entry
    fn parse_accounts(
        &mut self,
        yaml: &Yaml,
        expected_addresses: Option<&HashSet<&Address>>,
    ) -> Result<HashMap<Address, AccountMatch>> {
        let mut accounts = HashMap::new();
        for (address, account) in yaml.as_hash().context("parse_hash")?.iter() {
            let acc_storage = &account["storage"];
            let acc_balance = &account["balance"];
            let acc_code = &account["code"];
            let acc_nonce = &account["nonce"];

            let mut storage = HashMap::new();
            if !acc_storage.is_badvalue() {
                for (slot, value) in account["storage"].as_hash().context("parse_hash")?.iter() {
                    storage.insert(Self::parse_u256(slot)?, Self::parse_u256(value)?);
                }
            }

            let address = Self::parse_address(address, expected_addresses)?;
            let account = AccountMatch {
                address,
                balance: if acc_balance.is_badvalue() {
                    None
                } else {
                    Some(Self::parse_u256(acc_balance)?)
                },
                code: if acc_code.is_badvalue() {
                    None
                } else {
                    Some(self.parse_code(acc_code)?)
                },
                nonce: if acc_nonce.is_badvalue() {
                    None
                } else {
                    Some(Self::parse_u256(acc_nonce)?)
                },
                storage,
            };
            accounts.insert(address, account);
        }
        Ok(accounts)
    }

    /// converts list of tagged values string into a map
    /// if there's no tags, an entry with an empty tag and the full string is
    /// returned
    fn decompose_tags(expr: &str) -> HashMap<String, String> {
        let mut tags = HashMap::new();
        let mut it = expr.trim();
        if it.is_empty() {
            tags.insert("".to_string(), "".to_string());
        } else {
            while !it.is_empty() {
                if it.starts_with(':') {
                    let tag = &it[..it.find([' ', '\n']).expect("unable to find end tag")];
                    it = &it[tag.len() + 1..];
                    let value_len = if tag == ":yul" || tag == ":solidity" {
                        it.len()
                    } else {
                        it.find(':').unwrap_or(it.len())
                    };
                    tags.insert(tag.to_string(), it[..value_len].trim().to_string());
                    it = &it[value_len..];
                } else {
                    tags.insert("".to_string(), it.trim().to_string());
                    it = &it[it.len()..]
                }
            }
        }
        tags
    }
    /// returns the element as an address
    fn parse_address(
        yaml: &Yaml,
        expected_addresses: Option<&HashSet<&Address>>,
    ) -> Result<Address> {
        if let Some(as_str) = yaml.as_str() {
            parse::parse_address(as_str)
        } else if let Some(as_i64) = yaml.as_i64() {
            // Try to convert the integer to hex if has expected addresses for
            // accounts after transaction (result).
            // e.g. 10 -> 0xa
            if let Some(expected_addresses) = expected_addresses {
                let address = Address::from_slice(&hex::decode(format!("{as_i64:040x}"))?);
                if expected_addresses.contains(&address) {
                    return Ok(address);
                }
            }

            // Format as a hex string directly for accounts before transaction (pre).
            // e.g. 10 -> 0x10
            Ok(Address::from_slice(&hex::decode(format!("{as_i64:0>40}"))?))
        } else if let Yaml::Real(as_real) = yaml {
            Ok(Address::from_str(as_real)?)
        } else {
            bail!("cannot parse address {yaml:?}");
        }
    }

    /// returns the element as a to address
    fn parse_to_address(yaml: &Yaml) -> Result<Option<Address>> {
        if let Some(as_str) = yaml.as_str() {
            if as_str.trim().is_empty() {
                return Ok(None);
            }
            parse::parse_to_address(as_str)
        } else {
            bail!("cannot parse to address {:?}", yaml);
        }
    }

    /// returns the element as an array of bytes
    fn parse_bytes(yaml: &Yaml) -> Result<Bytes> {
        let as_str = yaml.as_str().context("bytes_as_str")?;
        parse::parse_bytes(as_str)
    }

    /// returns the element as calldata bytes, supports 0x, :raw, :abi, :yul and
    /// { LLL }
    fn parse_calldata(&mut self, yaml: &Yaml) -> Result<parse::Calldata> {
        if let Some(as_str) = yaml.as_str() {
            return parse::parse_calldata(self.compiler, as_str, &None);
        }
        if let Some(as_map) = yaml.as_hash() {
            if let Some(Yaml::String(data)) = as_map.get(&Yaml::String("data".to_string())) {
                let raw_access_list =
                    parse_raw_access_list(as_map.get(&Yaml::String("accessList".to_string())))?;
                return parse::parse_calldata(self.compiler, data, &raw_access_list);
            } else {
                bail!("do not know what to do with calldata(3): {:?}", yaml);
            }
        }
        bail!("do not know what to do with calldata(4): {:?}", yaml);
    }

    /// parse entry as code, can be 0x, :raw, :yul or { LLL }
    fn parse_code(&mut self, yaml: &Yaml) -> Result<Bytes> {
        let as_str = if let Some(as_str) = yaml.as_str() {
            as_str.to_string()
        } else if let Some(as_int) = yaml.as_i64() {
            format!("0x{as_int:x}")
        } else {
            bail!(format!("code '{yaml:?}' not an str"));
        };
        parse::parse_code(self.compiler, &as_str)
    }

    /// parse a hash entry
    fn parse_hash(yaml: &Yaml) -> Result<H256> {
        let value = yaml.as_str().context("not a str")?;
        parse::parse_hash(value)
    }

    /// parse an uint256 entry
    fn parse_u256(yaml: &Yaml) -> Result<U256> {
        if let Some(as_int) = yaml.as_i64() {
            Ok(U256::from(as_int))
        } else if let Some(as_str) = yaml.as_str() {
            parse::parse_u256(as_str)
        } else if yaml.as_f64().is_some() {
            if let Yaml::Real(value) = yaml {
                Ok(U256::from_str_radix(value, 10)?)
            } else {
                unreachable!()
            }
        } else {
            bail!("parse_u256 {:?}", yaml)
        }
    }

    /// parse u64 entry
    #[allow(clippy::cast_sign_loss)]
    fn parse_u64(yaml: &Yaml) -> Result<u64> {
        if let Some(as_int) = yaml.as_i64() {
            Ok(as_int as u64)
        } else if let Some(as_str) = yaml.as_str() {
            parse::parse_u64(as_str)
        } else {
            bail!("parse_u64 {:?}", yaml)
        }
    }

    /// parse a unique or a list of references,
    ///   -1 => Ref::Any
    ///   a int value => Ref::Index(value)
    ///   :label xxx => Ref::Label(value)
    ///   <range_lo>-<range_hi> >= Ref::Index(range_lo)..=RefIndex(range_hi)
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn parse_refs(yaml: &Yaml) -> Result<Refs> {
        if yaml.is_badvalue() {
            // It is considered as Any if missing this field.
            return Ok(Refs(vec![Ref::Any]));
        }

        // convert a unique element into a list
        let yamls = if yaml.is_array() {
            yaml.as_vec().context("as_vec")?.iter().collect()
        } else {
            vec![yaml]
        };

        let mut refs = Vec::new();

        for yaml in yamls {
            if let Some(as_int) = yaml.as_i64() {
                // index or any
                if as_int == -1 {
                    refs.push(Ref::Any)
                } else {
                    refs.push(Ref::Index(as_int as usize))
                }
            } else if let Some(as_str) = yaml.as_str() {
                let tags = Self::decompose_tags(as_str);
                if let Some(label) = tags.get(":label") {
                    // label
                    refs.push(Ref::Label(label.to_string()))
                } else if as_str.contains('-') {
                    // range
                    let mut range = as_str.splitn(2, '-');
                    let lo = range.next().unwrap().parse::<usize>()?;
                    let hi = range.next().unwrap().parse::<usize>()?;
                    for i in lo..=hi {
                        refs.push(Ref::Index(i))
                    }
                } else {
                    bail!("parse_ref(1) {:?}", yaml);
                }
            } else {
                bail!("parse_ref(2) {:?}", yaml);
            };
        }
        Ok(Refs(refs))
    }
}

fn parse_raw_access_list(access_list: Option<&Yaml>) -> Result<Option<parse::RawAccessList>> {
    if let Some(Yaml::Array(access_items)) = access_list {
        let access_list = access_items
            .iter()
            .map(|item| {
                item.as_hash().map_or(
                    Err(anyhow!("Parsed access list item must be a hash")),
                    |item| {
                        let address = match item.get(&Yaml::String("address".to_string())) {
                            Some(Yaml::Integer(i)) => format!("{i:x}"),
                            Some(Yaml::String(s)) => {
                                assert!(s.starts_with("0x"));
                                s[2..].to_string()
                            }
                            val => bail!("Failed to parse access list address = {val:?}"),
                        };
                        let address = format!("0x{:0>40}", address);

                        let storage_keys = if let Some(Yaml::Array(storage_keys)) =
                            item.get(&Yaml::String("storageKeys".to_string()))
                        {
                            storage_keys
                                .iter()
                                .map(|key| {
                                    let key = match key {
                                        Yaml::Integer(i) => format!("{i:x}"),
                                        Yaml::String(s) => {
                                            assert!(s.starts_with("0x"));
                                            s[2..].to_string()
                                        }
                                        val => bail!(
                                            "Failed to parse access list storage key = {val:?}"
                                        ),
                                    };
                                    Ok(format!("0x{:0>64}", key))
                                })
                                .collect::<Result<_>>()?
                        } else {
                            bail!("Parsed access list storage keys must be an array");
                        };

                        Ok(parse::RawAccessListItem::new(address, storage_keys))
                    },
                )
            })
            .collect::<Result<_>>()?;

        return Ok(Some(access_list));
    }

    Ok(None)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        config::TestSuite,
        statetest::{run_test, CircuitsConfig, StateTestError},
    };
    use eth_types::{address, AccessList, AccessListItem};

    const TEMPLATE: &str = r#"
arith:
  env:
    currentCoinbase: 2adc25665018aa1fe0e6bc666dac8fc2697ff9ba
    currentDifficulty: 0x20000
    currentGasLimit: {{ gas_limit }}
    currentNumber: 1
    currentTimestamp: 1000
    previousHash: 5e20a0453cecd065ea59c37ac63e079ee08998b6045136a8ce6635c7912ec0b6
  pre:
    cccccccccccccccccccccccccccccccccccccccc:
      balance: 1000000000000
      code: :raw {{ pre_code }}
      nonce: '0'
      storage:
        0 : 0x01
    a94f5374fce5edbc8e2a8697c15331677e6ebf0b:
      balance: 1000000000000
      code: '0x'
      nonce: '0'
      storage: {}
  transaction:
    data:
    - :raw 0x00
    - data: :label data1 :raw 0x01
      accessList:
      - address: 0xF00000000000000000000000000000000000F101
        storageKeys:
        - 0x60A7
        - 0xBEEF
    gasLimit:
    - '80000000'
    - '80000001'
    gasPrice: '10'
    nonce: '0'
    to: cccccccccccccccccccccccccccccccccccccccc
    value:
    - '1'
    - '2'
    secretKey: "45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8"
  expect:
    - indexes:
        data: :label data1
        gas:  !!int 1
        value: !!int 1
      network:
        - '>=Istanbul'
      result:
        cccccccccccccccccccccccccccccccccccccccc:
          balance: 10

    - indexes:
        data: !!int -1
        gas:  !!int 1
        value: !!int -1
      network:
        - '>=Istanbul'
      result:
        cccccccccccccccccccccccccccccccccccccccc:
          balance: 20

    - indexes:
        data: !!int -1
        gas:  !!int -1
        value: !!int 1
      network:
        - '>=Istanbul'
      result:
        cccccccccccccccccccccccccccccccccccccccc:
          balance: 30

    - indexes:
        data: !!int -1
        gas:  !!int -1
        value: !!int -1
      network:
        - '>=Istanbul'
      expectException:
        '{{ expect_exception_network }}' : TR_TypeNotSupported
      result:
        cccccccccccccccccccccccccccccccccccccccc:
          balance: {{ res_balance }}
          nonce: {{ res_nonce }}
          code: {{ res_code }}
          storage:
            0: {{ res_storage }}


"#;
    struct Template {
        gas_limit: String,
        pre_code: String,
        res_storage: String,
        res_balance: String,
        res_code: String,
        res_nonce: String,
        res_exception: bool,
    }

    impl Default for Template {
        fn default() -> Self {
            Self {
                gas_limit: "100000000".into(),
                pre_code: ":raw 0x600100".into(),
                res_storage: "0x01".into(),
                res_balance: "1000000000001".into(),
                res_code: ":raw 0x600100".into(),
                res_nonce: "0".into(),
                res_exception: false,
            }
        }
    }
    impl ToString for Template {
        fn to_string(&self) -> String {
            TEMPLATE
                .replace("{{ gas_limit }}", &self.gas_limit)
                .replace("{{ pre_code }}", &self.pre_code)
                .replace("{{ res_storage }}", &self.res_storage)
                .replace("{{ res_balance }}", &self.res_balance)
                .replace("{{ res_code }}", &self.res_code)
                .replace("{{ res_nonce }}", &self.res_nonce)
                .replace(
                    "{{ expect_exception_network }}",
                    if self.res_exception {
                        ">=Istanbul"
                    } else {
                        "Istanbul"
                    },
                )
        }
    }

    #[test]
    fn combinations() -> Result<()> {
        let tcs = YamlStateTestBuilder::new(&Compiler::default())
            .load_yaml("", &Template::default().to_string())?
            .into_iter()
            .map(|v| (v.id.clone(), v))
            .collect::<HashMap<_, _>>();

        assert_eq!(tcs.len(), 8);

        let ccccc = address!("cccccccccccccccccccccccccccccccccccccccc");
        let check_ccccc_balance = |id: &str, v: u64| {
            assert_eq!(tcs[id].result[&ccccc].balance, Some(U256::from(v)), "{id}")
        };

        check_ccccc_balance("arith_d0_g0_v0", 1000000000001);
        check_ccccc_balance("arith_d1(data1)_g1_v1", 10);
        check_ccccc_balance("arith_d0_g1_v0", 20);
        check_ccccc_balance("arith_d0_g0_v1", 30);
        Ok(())
    }

    #[test]
    fn test_yaml_parse() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default())
            .load_yaml("", &Template::default().to_string())?;

        // Check the last test.
        let current = tc.pop().unwrap();

        let a94f5 = address!("a94f5374fce5edbc8e2a8697c15331677e6ebf0b");
        let ccccc = address!("cccccccccccccccccccccccccccccccccccccccc");

        let expected = StateTest {
            path: "".into(),
            id: "arith_d1(data1)_g1_v1".into(),
            env: Env {
                current_base_fee: U256::from(DEFAULT_BASE_FEE),
                current_coinbase: address!("0x2adc25665018aa1fe0e6bc666dac8fc2697ff9ba"),
                current_difficulty: U256::from(0x20000u64),
                current_number: 1,
                current_timestamp: 1000,
                current_gas_limit: 100000000,
                previous_hash: H256::from_slice(&hex::decode(
                    "5e20a0453cecd065ea59c37ac63e079ee08998b6045136a8ce6635c7912ec0b6",
                )?),
            },
            secret_key: Bytes::from(hex::decode(
                "45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8",
            )?),
            from: a94f5,
            to: Some(ccccc),
            gas_limit: 80000001,
            gas_price: U256::from(10u64),
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
            nonce: U256::zero(),
            value: U256::from(2),
            data: Bytes::from(&[1]),
            access_list: Some(AccessList(vec![AccessListItem {
                address: address!("0xf00000000000000000000000000000000000f101"),
                storage_keys: vec![H256::from_low_u64_be(0x60a7), H256::from_low_u64_be(0xbeef)],
            }])),
            pre: BTreeMap::from([
                (
                    ccccc,
                    Account {
                        address: ccccc,
                        balance: U256::from(1000000000000u64),
                        code: Bytes::from(&[0x60, 0x01, 0x00]),
                        nonce: U256::zero(),
                        storage: HashMap::from([(U256::zero(), U256::one())]),
                    },
                ),
                (
                    a94f5,
                    Account {
                        address: a94f5,
                        balance: U256::from(1000000000000u64),
                        code: Bytes::default(),
                        nonce: U256::zero(),
                        storage: HashMap::new(),
                    },
                ),
            ]),
            result: HashMap::from([(
                ccccc,
                AccountMatch {
                    address: ccccc,
                    balance: Some(U256::from(10u64)),
                    nonce: None,
                    code: None,
                    storage: HashMap::new(),
                },
            )]),
            exception: false,
        };

        assert_eq!(current, expected);
        Ok(())
    }

    #[test]
    fn result_pass() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default())
            .load_yaml("", &Template::default().to_string())?;
        let t1 = tc.remove(0);
        run_test(t1, TestSuite::default(), CircuitsConfig::default())?;
        Ok(())
    }
    #[test]
    fn test_result_bad_storage() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                res_storage: "2".into(),
                ..Default::default()
            }
            .to_string(),
        )?;
        assert_eq!(
            run_test(
                tc.remove(0),
                TestSuite::default(),
                CircuitsConfig::default()
            ),
            Err(StateTestError::StorageMismatch {
                slot: U256::from(0u8),
                expected: U256::from(2u8),
                found: U256::from(1u8)
            })
        );

        Ok(())
    }
    #[test]
    fn bad_balance() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                res_balance: "1000000000002".into(),
                ..Default::default()
            }
            .to_string(),
        )?;
        assert_eq!(
            run_test(
                tc.remove(0),
                TestSuite::default(),
                CircuitsConfig::default()
            ),
            Err(StateTestError::BalanceMismatch {
                expected: U256::from(1000000000002u64),
                found: U256::from(1000000000001u64)
            })
        );

        Ok(())
    }

    #[test]
    fn bad_code() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                res_code: ":raw 0x600200".into(),
                ..Default::default()
            }
            .to_string(),
        )?;
        assert_eq!(
            run_test(
                tc.remove(0),
                TestSuite::default(),
                CircuitsConfig::default()
            ),
            Err(StateTestError::CodeMismatch {
                expected: Bytes::from(&[0x60, 0x02, 0x00]),
                found: Bytes::from(&[0x60, 0x01, 0x00])
            })
        );

        Ok(())
    }

    #[test]
    fn bad_nonce() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                res_nonce: "2".into(),
                ..Default::default()
            }
            .to_string(),
        )?;

        assert_eq!(
            run_test(
                tc.remove(0),
                TestSuite::default(),
                CircuitsConfig::default()
            ),
            Err(StateTestError::NonceMismatch {
                expected: U256::from(2),
                found: U256::from(0)
            })
        );

        Ok(())
    }

    #[test]
    fn sstore() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                pre_code: ":raw 0x607760005500".into(),
                res_code: ":raw 0x607760005500".into(),
                res_storage: "0x77".into(),
                ..Default::default()
            }
            .to_string(),
        )?;
        let config = CircuitsConfig::default();
        run_test(tc.remove(0), TestSuite::default(), config)?;
        Ok(())
    }

    #[test]
    fn marked_as_exception_and_fails() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                gas_limit: "2300".into(),
                res_exception: true,
                ..Default::default()
            }
            .to_string(),
        )?;
        let config = CircuitsConfig::default();
        run_test(tc.remove(0), TestSuite::default(), config).expect("Should pass");
        Ok(())
    }
    #[test]
    fn marked_as_exception_but_does_not_fail() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                res_exception: true,
                ..Default::default()
            }
            .to_string(),
        )?;
        let config = CircuitsConfig::default();
        let res = run_test(tc.remove(0), TestSuite::default(), config);
        assert!(res.is_err());
        Ok(())
    }

    #[cfg(feature = "warn-unimplemented")]
    #[test]
    fn fail_bad_code() -> Result<()> {
        let mut tc = YamlStateTestBuilder::new(&Compiler::default()).load_yaml(
            "",
            &Template {
                pre_code: ":raw 0xF4".into(),
                res_code: ":raw 0xF4".into(),
                res_storage: "0x77".into(),
                ..Default::default()
            }
            .to_string(),
        )?;
        assert!(run_test(
            tc.remove(0),
            TestSuite::default(),
            CircuitsConfig::default()
        )
        .is_err());
        Ok(())
    }
}
