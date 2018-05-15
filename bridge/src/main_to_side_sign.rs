use futures::{Async, Future, Poll, Stream};
use futures::future::{join_all, FromErr, JoinAll};
use tokio_timer::Timeout;
use web3::{self, Transport};
use web3::types::{Bytes, H256, Log, U256, TransactionReceipt};
use web3::helpers::CallResult;
use ethabi::RawLog;
use error::{self, ResultExt};
use contracts::home::HomeBridge;
use contracts::foreign::ForeignBridge;
use contract_connection::ContractConnection;
use relay_stream::LogToFuture;
use side_contract::{IsMainToSideSignedOnSide, SideContract};
use helpers::Transaction;

/// takes `deposit_log` which must be a `HomeBridge.Deposit` event
/// and returns the payload for the call to `ForeignBridge.deposit()`
fn deposit_relay_payload(web3_log: Log) -> Vec<u8> {
    let tx_hash = web3_log
        .transaction_hash
        .expect("log must be mined and contain `transaction_hash`. q.e.d.");
    let raw_ethabi_log = RawLog {
        topics: web3_log.topics,
        data: web3_log.data.0,
    };
    let ethabi_log = HomeBridge::default()
        .events()
        .deposit()
        .parse_log(raw_ethabi_log)
        .expect("log must be a from a Deposit event. q.e.d.");
}

enum State<T: Transport> {
    AwaitHasSigned(Timeout<IsMainToSideSignedOnSide<T>>),
    AwaitTxSent(Timeout<Transaction<T>>),
    AwaitTxReceipt(Timeout<FromErr<CallResult<Option<TransactionReceipt>, T::Out>, error::Error>>),
    HasAlreadySigned,
}

/// `Future` responsible for doing a single relay from `main` to `side`
pub struct MainToSideSign<T: Transport> {
    main_tx_hash: H256,
    state: State<T>,
    side: SideContract<T>,
}

impl<T: Transport> MainToSideSign<T> {
    pub fn new(log: Log, side: SideContract<T>) -> Self {
        let main_tx_hash = log.transaction_hash
            .expect("`log` must be mined and contain `transaction_hash`. q.e.d.");
        info!("{:?} - step 1/3 - about to check whether it is already relayed", main_tx_hash);

        let future = side.is_main_to_side_signed_on_side(main_tx_hash, side.authority_address);
        let state = State::AwaitCheckIsAlreadyRelayed(future);

        Self { main_tx_hash, side, state }
    }
}

impl<T: Transport> Future for MainToSideSign<T> {
    type Item = TransactionReceipt;
    type Error = error::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let next_state = match self.state {
                State::AwaitHasAlreadySigned(ref mut future) => {
                    if try_ready!(future) {
                        State::HasAlreadySigned()
                    } else {
                        State::AwaitTxSent(
                            self.options.side_contract.sign_main_to_side(
                                self.recipient,
                                self.value,
                                self.main_tx_hash))
                    }
                }
                State::AwaitTxSent(ref mut future) => {
                    let side_tx_hash = try_ready!(
                        future
                            .poll()
                            .chain_err(|| "MainToSideSign: checking whether {} already was relayed failed", self.main_tx_hash)
                    );
                    State::AwaitTxReceipt(web3::api::Eth::new(self.options.side_contract.transport)
                        .transaction_receipt(side_tx_hash))
                }
                State::AwaitTxReceipt(ref mut future) => {
                    let receipt = try_ready!(
                        future
                            .poll()
                            .chain_err(|| "MainToSideSign: checking whether {} already was relayed failed", self.main_tx_hash)
                    );
                    info!(
                        "{:?} - step 2/2 - DONE - transaction sent {:?}",
                        self.main_tx_hash, receipt.transaction_hash
                    );

                    return Ok(Async::Ready(receipt));
                }
            };
            self.state = next_state;
        }
    }
}

/// options for relays from side to main
#[derive(Clone)]
pub struct LogToMainToSideSign<T> {
    pub side: SideContract<T>,
}

/// from the options and a log a relay future can be made
impl<T: Transport> LogToFuture for LogToMainToSideSign<T> {
    type Future = MainToSideSign<T>;

    fn log_to_future(&self, log: Log) -> Self::Future {
        MainToSideSign::new(log, self.side.clone())
    }
}

#[cfg(test)]
mod tests {
    use rustc_hex::FromHex;
    use web3::types::{Bytes, Log};
    use super::*;
    use tokio_core::reactor::Core;
    use contracts;
    use ethabi;
    use rustc_hex::ToHex;

    #[test]
    fn test_deposit_relay_payload() {
        let data = "000000000000000000000000aff3454fce5edbc8cca8697c15331677e6ebcccc00000000000000000000000000000000000000000000000000000000000000f0".from_hex().unwrap();
        let log = Log {
            data: data.into(),
            topics: vec![
                "e1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c".into(),
            ],
            transaction_hash: Some(
                "884edad9ce6fa2440d8a54cc123490eb96d2768479d49ff9c7366125a9424364".into(),
            ),
            ..Default::default()
        };

        let payload = deposit_relay_payload(log);
        let expected: Vec<u8> = "26b3293f000000000000000000000000aff3454fce5edbc8cca8697c15331677e6ebcccc00000000000000000000000000000000000000000000000000000000000000f0884edad9ce6fa2440d8a54cc123490eb96d2768479d49ff9c7366125a9424364".from_hex().unwrap();
        assert_eq!(expected, payload);
    }

    #[test]
    fn test_deposit_relay_future() {
        let deposit_topic = HomeBridge::default()
            .events()
            .deposit()
            .create_filter()
            .topic0;

        let log = contracts::home::logs::Deposit {
            recipient: "aff3454fce5edbc8cca8697c15331677e6ebcccc".into(),
            value: 1000.into(),
        };

        // TODO [snd] would be great if there were a way to automate this
        let log_data = ethabi::encode(&[
            ethabi::Token::Address(log.recipient),
            ethabi::Token::Uint(log.value),
        ]);

        let log_tx_hash =
            "0x884edad9ce6fa2440d8a54cc123490eb96d2768479d49ff9c7366125a9424364".into();

        let raw_log = Log {
            address: "0000000000000000000000000000000000000001".into(),
            topics: deposit_topic.into(),
            data: Bytes(log_data),
            transaction_hash: Some(log_tx_hash),
            ..Default::default()
        };

        let authority_address = "0000000000000000000000000000000000000001".into();

        let tx_hash = "0x1db8f385535c0d178b8f40016048f3a3cffee8f94e68978ea4b277f57b638f0b";
        let foreign_contract_address = "0000000000000000000000000000000000000dd1".into();

        let tx_data = ForeignBridge::default().functions().deposit().input(
            log.recipient,
            log.value,
            log_tx_hash,
        );

        let transport = mock_transport!(
            "eth_sendTransaction" =>
                req => json!([{
                    "data": format!("0x{}", tx_data.to_hex()),
                    "from": "0x0000000000000000000000000000000000000001",
                    "gas": "0xfd",
                    "gasPrice": "0xa0",
                    "to": foreign_contract_address,
                }]),
            res => json!(tx_hash);
        );

        let connection = ContractConnection::new(
            authority_address,
            foreign_contract_address,
            transport.clone(),
            ::std::time::Duration::from_secs(1),
        );

        let options = Options {
            foreign: connection,
            gas: 0xfd.into(),
            gas_price: 0xa0.into(),
        };

        let future = MainToSideSign::new(raw_log, options);

        let mut event_loop = Core::new().unwrap();
        let result = event_loop.run(future).unwrap();
        assert_eq!(result, tx_hash.into());

        assert_eq!(transport.actual_requests(), transport.expected_requests());
    }
}