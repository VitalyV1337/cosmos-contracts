use std::str::FromStr;

use cw_multi_test::Contract;
use ::prost::Message;

use cosmwasm_std::{
    to_binary, BankMsg, Deps, DepsMut, Env, MessageInfo, Reply, Response, SubMsg, SubMsgResponse,
    SubMsgResult, WasmMsg, Coin, Decimal, Uint128,
};
use cw_utils::one_coin;
use osmosis_router::{
    router::{build_swap_msg, get_swap_amount_out_response},
    OsmosisSwapMsg,
};
use osmosis_std::types::osmosis::poolmanager::v1beta1::PoolmanagerQuerier;
use error::OsmosisRouterError;

use crate::{
    msg::{
        AfterSwapAction, ExecuteMsg, MsgReplyId, MsgTransfer, MsgTransferResponse, MultiSwapMsg,
        PriceImpactTradeResponse,
    },
    state::{
        load_ibc_transfer_reply_state, load_multi_swap_state, load_swap_reply_state,
        remove_multi_swap_state, store_awaiting_ibc_transfer, store_ibc_transfer_reply_state,
        store_multi_swap_state, store_swap_reply_state, swap_reply_state_exists,
        IbcTransferReplyState, MultiSwapState, SwapReplyState,
    },
    ContractError,
};

const TRANSFER_PORT: &'static str = "transfer";
const IBC_CALLBACK: &'static str = "ibc_callback";
const IBC_PACKET_LIFITIME: u64 = 604_800u64;

pub fn swap(
    deps: DepsMut,
    env: &Env,
    info: &MessageInfo,
    swap_msg: OsmosisSwapMsg,
    after_swap_action: AfterSwapAction,
    local_fallback_address: String,
) -> Result<Response, ContractError> {
    // re-entrancy check
    if swap_reply_state_exists(deps.storage)? {
        return Err(ContractError::ContractLocked {
            msg: "Another swap in process already".to_owned(),
        });
    }

    let input_coin = one_coin(info)?;
    let swap_msg = build_swap_msg(deps.storage, env, input_coin, swap_msg)?;

    store_swap_reply_state(
        deps.storage,
        &SwapReplyState {
            after_swap_action,
            local_fallback_address,
        },
    )?;

    Ok(Response::new().add_submessage(SubMsg::reply_on_success(swap_msg, MsgReplyId::Swap.repr())))
}

pub fn handle_after_swap_action(
    deps: DepsMut,
    env: &Env,
    reply: Reply,
) -> Result<Response, ContractError> {
    let output_token_info = get_swap_amount_out_response(deps.storage, reply)?;
    let after_swap_info = load_swap_reply_state(deps.storage)?;

    let response = match after_swap_info.after_swap_action {
        AfterSwapAction::BankSend { receiver } => {
            let bank = BankMsg::Send {
                to_address: receiver,
                amount: vec![output_token_info.output_coin],
            };
            Response::new().add_message(bank)
        }
        AfterSwapAction::CustomCall {
            contract_address,
            msg,
        } => {
            let wasm = WasmMsg::Execute {
                contract_addr: contract_address,
                msg: to_binary(&msg)?,
                funds: vec![output_token_info.output_coin],
            };
            Response::new().add_message(wasm)
        }
        AfterSwapAction::IbcTransfer {
            receiver,
            channel,
            next_memo,
        } => {
            let next_memo = next_memo.unwrap_or_else(|| serde_json_wasm::from_str("{}").unwrap());
            let next_memo = {
                let serde_cw_value::Value::Map(mut m) = next_memo.0 else { unreachable!() };
                m.insert(
                    serde_cw_value::Value::String(IBC_CALLBACK.to_owned()),
                    serde_cw_value::Value::String(env.contract.address.to_string()),
                );
                serde_cw_value::Value::Map(m)
            };

            let memo = serde_json_wasm::to_string(&next_memo)
                .map_err(|_e| ContractError::InvalidMemo {})?;

            let ibc_transfer = MsgTransfer {
                source_port: TRANSFER_PORT.to_owned(),
                source_channel: channel.clone(),
                token: Some(output_token_info.output_coin.clone().into()),
                sender: env.contract.address.to_string(),
                receiver,
                timeout_height: None,
                timeout_timestamp: Some(env.block.time.plus_seconds(IBC_PACKET_LIFITIME).nanos()),
                memo,
            };

            store_ibc_transfer_reply_state(
                deps.storage,
                &IbcTransferReplyState {
                    local_fallback_address: after_swap_info.local_fallback_address,
                    channel,
                    denom: output_token_info.output_coin.denom,
                    amount: output_token_info.output_coin.amount,
                },
            )?;

            Response::new().add_submessage(SubMsg::reply_on_success(
                ibc_transfer,
                MsgReplyId::IbcTransfer.repr(),
            ))
        }
    };

    Ok(response)
}

pub fn handle_ibc_transfer_reply(deps: DepsMut, reply: Reply) -> Result<Response, ContractError> {
    let SubMsgResult::Ok(SubMsgResponse { data: Some(b), .. }) = reply.result else {
        return Err(ContractError::FailedIBCTransfer { msg: format!("failed reply: {:?}", reply.result) })
    };

    let ibc_transfer_response =
        MsgTransferResponse::decode(&b[..]).map_err(|_e| ContractError::FailedIBCTransfer {
            msg: format!("could not decode response: {b}"),
        })?;

    let ibc_transfer_info = load_ibc_transfer_reply_state(deps.storage)?;
    store_awaiting_ibc_transfer(
        deps.storage,
        ibc_transfer_response.sequence,
        &ibc_transfer_info,
    )?;

    Ok(Response::new())
}

pub fn handle_multiswap(
    deps: DepsMut,
    env: &Env,
    mut swaps: Vec<MultiSwapMsg>,
    local_fallback_address: String,
) -> Result<Response, ContractError> {
    if swaps.is_empty() {
        return Err(ContractError::InvalidAmountOfSwaps {});
    }

    // store multi-swap information
    swaps.reverse();
    store_multi_swap_state(
        deps.storage,
        &MultiSwapState {
            swaps,
            local_fallback_address,
        },
    )?;

    // handle first swap and initiate callback loop
    handle_multiswap_reply(deps, env)
}

pub fn handle_multiswap_reply(deps: DepsMut, env: &Env) -> Result<Response, ContractError> {
    let mut multi_swaps = load_multi_swap_state(deps.storage)?;
    if multi_swaps.swaps.is_empty() {
        // all swaps are done, remove state and return ok
        remove_multi_swap_state(deps.storage);
        return Ok(Response::new());
    }

    let next_swap = multi_swaps.swaps.pop().unwrap();
    let swap_msg = WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_binary(&ExecuteMsg::SwapWithAction {
            swap_msg: next_swap.swap_msg,
            after_swap_action: next_swap.after_swap_action,
            local_fallback_address: multi_swaps.local_fallback_address.clone(),
        })?,
        funds: vec![next_swap.amount_in],
    };

    store_multi_swap_state(deps.storage, &multi_swaps)?;
    Ok(Response::new().add_submessage(SubMsg::reply_on_success(
        swap_msg,
        MsgReplyId::MultiSwap.repr(),
    )))
}

pub fn estimate_price_impact_twap_min_input_output(
    deps: Deps,
    env: &Env,
    input_coin: Coin,
    to_coin_denom: String,
    pool_id: u64,
    max_price_impact: Decimal,
    twap_price: Decimal,
) -> Result <PriceImpactTradeResponse, ContractError> {

    // // Define your querier
    let poolmanager_querier = PoolmanagerQuerier::new(&deps.querier);

    // Get the pool based on ID
    let pool = poolmanager_querier.pool(pool_id)?;

    let spot_price_response = poolmanager_querier.spot_price(pool_id, input_coin.denom.clone(), to_coin_denom.clone())?;
    let spot_price_str = spot_price_response.spot_price; // Assuming spot_price is a string in the response
    let spot_price = Decimal::from_str(&spot_price_str).map_err(|_| ContractError::InvalidSpotPrice)?; 

    // Calculate adjusted maxPriceImpact based on twapPrice and spotPrice
    let price_deviation = (spot_price - twap_price) / twap_price;
    max_price_impact = max_price_impact - price_deviation;

    loop {
        // Calculate token out
        let estimate_response = poolmanager_querier.estimate_single_pool_swap_exact_amount_in(pool_id, input_coin.denom.clone(), to_coin_denom.clone())?;
        let token_out = estimate_response.token_out_amount; // Assuming token_out is a field in the response

        // If token_out is zero, return an error
        if token_out.amount.is_zero() {
            return Err(ContractError::ZeroTokenOut);
        }

        let curr_trade_price = token_out.amount / input_coin.amount;
        let price_deviation = (spot_price - curr_trade_price) / curr_trade_price;

        if price_deviation <= max_price_impact {
            return Ok(PriceImpactTradeResponse{
                amount_in: input_coin.clone(),
                amount_out: token_out,
            });
        } else {
            // Half the input amount and try again
            input_coin.amount = input_coin.amount / Uint128::from(2u64);
        }
    }

    // Ok(PriceImpactTradeResponse{
    //     amount_in: input_coin.clone(),
    //     amount_out: input_coin,
    // })
}