// Copyright 2022 ComposableFi
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//      http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
	core::{
		ics02_client::{
			context::{ClientKeeper, ClientTypes},
			handler::dispatch as ics2_msg_dispatcher,
		},
		ics03_connection::handler::dispatch as ics3_msg_dispatcher,
		ics04_channel::{
			handler::{
				channel_callback as ics4_callback, channel_dispatch as ics4_msg_dispatcher,
				channel_validate as ics4_validate, get_module_for_packet_msg,
				packet_callback as ics4_packet_callback,
				packet_dispatch as ics4_packet_msg_dispatcher, recv_packet::RecvPacketResult,
			},
			packet::PacketResult,
		},
		ics26_routing::{
			context::{Ics26Context, ModuleOutputBuilder, ReaderContext},
			error::Error,
			msgs::Ics26Envelope::{self, Ics2Msg, Ics3Msg, Ics4ChannelMsg, Ics4PacketMsg},
		},
	},
	events::IbcEvent,
	handler::HandlerOutput,
	prelude::*,
};
use core::fmt::Debug;
use ibc_proto::google::protobuf::Any;

/// Result of message execution - comprises of events emitted and logs entries created during the
/// execution of a transaction message.
pub struct MsgReceipt {
	pub events: Vec<IbcEvent>,
	pub log: Vec<String>,
}

/// Mimics the DeliverTx ABCI interface, but for a single message and at a slightly lower level.
/// No need for authentication info or signature checks here.
/// Returns a vector of all events that got generated as a byproduct of processing `message`.
pub fn deliver<Ctx>(ctx: &mut Ctx, message: Any) -> Result<MsgReceipt, Error>
where
	Ctx: Ics26Context + ReaderContext,
	Ics26Envelope<Ctx>: TryFrom<Any>,
	Error: From<<Ics26Envelope<Ctx> as TryFrom<Any>>::Error>,
{
	// Decode the proto message into a domain message, creating an ICS26 envelope.
	let envelope = decode::<Ctx>(message)?;

	// Process the envelope, and accumulate any events that were generated.
	let HandlerOutput { log, events, .. } = dispatch::<_>(ctx, envelope)?;

	Ok(MsgReceipt { events, log })
}

/// Attempts to convert a message into a [Ics26Envelope] message
pub fn decode<C>(message: Any) -> Result<Ics26Envelope<C>, Error>
where
	C: ClientTypes + Clone + Debug + PartialEq + Eq,
	Ics26Envelope<C>: TryFrom<Any>,
	Error: From<<Ics26Envelope<C> as TryFrom<Any>>::Error>,
{
	message.try_into().map_err(Into::into)
}

/// Top-level ICS dispatch function. Routes incoming IBC messages to their corresponding module.
/// Returns a handler output with empty result of type `HandlerOutput<()>` which contains the log
/// and events produced after processing the input `msg`.
/// If this method returns an error, the runtime is expected to rollback all state modifications to
/// the `Ctx` caused by all messages from the transaction that this `msg` is a part of.
pub fn dispatch<Ctx>(ctx: &mut Ctx, msg: Ics26Envelope<Ctx>) -> Result<HandlerOutput<()>, Error>
where
	Ctx: Ics26Context + ClientKeeper,
{
	let output = match msg {
		Ics2Msg(msg) => {
			let handler_output =
				ics2_msg_dispatcher::<Ctx>(ctx, msg).map_err(Error::ics02_client)?;

			// Apply the result to the context (host chain store).
			ctx.store_client_result(handler_output.result).map_err(Error::ics02_client)?;

			HandlerOutput::builder()
				.with_log(handler_output.log)
				.with_events(handler_output.events)
				.with_result(())
		},

		Ics3Msg(msg) => {
			let handler_output =
				ics3_msg_dispatcher::<_>(ctx, msg).map_err(Error::ics03_connection)?;

			// Apply any results to the host chain store.
			ctx.store_connection_result(handler_output.result)
				.map_err(Error::ics03_connection)?;

			HandlerOutput::builder()
				.with_log(handler_output.log)
				.with_events(handler_output.events)
				.with_result(())
		},

		Ics4ChannelMsg(msg) => {
			let module_id = ics4_validate(ctx, &msg).map_err(Error::ics04_channel)?;
			let (mut handler_builder, channel_result) =
				ics4_msg_dispatcher::<_>(ctx, &msg).map_err(Error::ics04_channel)?;

			let mut module_output = ModuleOutputBuilder::new();
			let cb_result =
				ics4_callback(ctx, &module_id, &msg, channel_result, &mut module_output);
			handler_builder.merge(module_output);
			let channel_result = cb_result.map_err(Error::ics04_channel)?;

			// Apply any results to the host chain store.
			ctx.store_channel_result(channel_result).map_err(Error::ics04_channel)?;

			handler_builder.with_result(())
		},

		Ics4PacketMsg(msg) => {
			let module_id = get_module_for_packet_msg(ctx, &msg).map_err(Error::ics04_channel)?;
			let (mut handler_builder, packet_result) =
				ics4_packet_msg_dispatcher::<_>(ctx, &msg).map_err(Error::ics04_channel)?;

			if matches!(packet_result, PacketResult::Recv(RecvPacketResult::NoOp)) {
				return Ok(handler_builder.with_result(()))
			}

			let mut module_output = ModuleOutputBuilder::new();
			let cb_result = ics4_packet_callback(ctx, &module_id, &msg, &mut module_output);
			handler_builder.merge(module_output);
			cb_result.map_err(Error::ics04_channel)?;

			// Apply any results to the host chain store.
			ctx.store_packet_result(packet_result).map_err(Error::ics04_channel)?;

			handler_builder.with_result(())
		},
	};

	Ok(output)
}

#[cfg(test)]
mod tests {
	use crate::prelude::*;

	use test_log::test;

	use crate::{
		applications::transfer::{
			context::test::deliver as ics20_deliver,
			msgs::transfer::{test_util::get_dummy_msg_transfer, MsgTransfer},
			packet::PacketData,
			PrefixedCoin, MODULE_ID_STR,
		},
		core::{
			ics02_client::msgs::{
				create_client::MsgCreateAnyClient, update_client::MsgUpdateAnyClient,
				upgrade_client::MsgUpgradeAnyClient, ClientMsg,
			},
			ics03_connection::msgs::{
				conn_open_ack::{test_util::get_dummy_raw_msg_conn_open_ack, MsgConnectionOpenAck},
				conn_open_init::{
					test_util::get_dummy_raw_msg_conn_open_init, MsgConnectionOpenInit,
				},
				conn_open_try::{test_util::get_dummy_raw_msg_conn_open_try, MsgConnectionOpenTry},
				ConnectionMsg,
			},
			ics04_channel::msgs::{
				chan_close_confirm::{
					test_util::get_dummy_raw_msg_chan_close_confirm, MsgChannelCloseConfirm,
				},
				chan_close_init::{
					test_util::get_dummy_raw_msg_chan_close_init, MsgChannelCloseInit,
				},
				chan_open_ack::{test_util::get_dummy_raw_msg_chan_open_ack, MsgChannelOpenAck},
				chan_open_init::{test_util::get_dummy_raw_msg_chan_open_init, MsgChannelOpenInit},
				chan_open_try::{test_util::get_dummy_raw_msg_chan_open_try, MsgChannelOpenTry},
				recv_packet::{test_util::get_dummy_raw_msg_recv_packet, MsgRecvPacket},
				timeout_on_close::{
					test_util::get_dummy_raw_msg_timeout_on_close, MsgTimeoutOnClose,
				},
				ChannelMsg, PacketMsg,
			},
		},
		events::IbcEvent,
		mock::client_state::{AnyClientState, AnyConsensusState},
	};

	use crate::{
		core::{
			ics24_host::identifier::ConnectionId,
			ics26_routing::{
				context::{Ics26Context, ModuleId, Router, RouterBuilder},
				error::Error,
				handler::dispatch,
				msgs::Ics26Envelope,
			},
		},
		handler::HandlerOutputBuilder,
		mock::{
			client_state::{MockClientState, MockConsensusState},
			context::{MockClientTypes, MockContext, MockRouterBuilder},
			header::{MockClientMessage, MockHeader},
		},
		test_utils::{get_dummy_account_id, DummyTransferModule},
		timestamp::Timestamp,
		Height,
	};

	#[test]
	#[ignore]
	/// These tests exercise two main paths: (1) the ability of the ICS26 routing module to dispatch
	/// messages to the correct module handler, and more importantly: (2) the ability of ICS
	/// handlers to work with the context and correctly store results (i.e., the `ClientKeeper`,
	/// `ConnectionKeeper`, and `ChannelKeeper` traits).
	fn routing_module_and_keepers() {
		#[derive(Clone, Debug)]
		enum TestMsg {
			Ics26(Ics26Envelope<MockContext<MockClientTypes>>),
			Ics20(MsgTransfer<PrefixedCoin>),
		}

		impl From<Ics26Envelope<MockContext<MockClientTypes>>> for TestMsg {
			fn from(msg: Ics26Envelope<MockContext<MockClientTypes>>) -> Self {
				Self::Ics26(msg)
			}
		}

		impl From<MsgTransfer<PrefixedCoin>> for TestMsg {
			fn from(msg: MsgTransfer<PrefixedCoin>) -> Self {
				Self::Ics20(msg)
			}
		}

		// Test parameters
		struct Test {
			name: String,
			msg: TestMsg,
			want_pass: bool,
		}
		let default_signer = get_dummy_account_id();
		let client_height = 5;
		let start_client_height = Height::new(0, client_height);
		let update_client_height = Height::new(0, 34);
		let update_client_height_after_send = Height::new(0, 35);

		let update_client_height_after_second_send = Height::new(0, 36);

		let upgrade_client_height = Height::new(1, 2);

		let upgrade_client_height_second = Height::new(1, 1);

		let transfer_module_id: ModuleId = MODULE_ID_STR.parse().unwrap();

		// We reuse this same context across all tests. Nothing in particular needs parametrizing.
		let mut ctx = {
			let ctx = MockContext::default();
			let module = DummyTransferModule::new(ctx.ibc_store_share());
			let router = MockRouterBuilder::default()
				.add_route(transfer_module_id.clone(), module)
				.unwrap()
				.build();
			ctx.with_router(router)
		};

		let create_client_msg = MsgCreateAnyClient::new(
			AnyClientState::from(MockClientState::new(MockClientMessage::Header(MockHeader::new(
				start_client_height,
			)))),
			AnyConsensusState::Mock(MockConsensusState::new(MockHeader::new(start_client_height))),
			default_signer.clone(),
		)
		.unwrap();

		//
		// Connection handshake messages.
		//
		let msg_conn_init =
			MsgConnectionOpenInit::try_from(get_dummy_raw_msg_conn_open_init()).unwrap();

		let correct_msg_conn_try = MsgConnectionOpenTry::try_from(get_dummy_raw_msg_conn_open_try(
			client_height,
			client_height,
		))
		.unwrap();

		// The handler will fail to process this msg because the client height is too advanced.
		let incorrect_msg_conn_try = MsgConnectionOpenTry::try_from(
			get_dummy_raw_msg_conn_open_try(client_height + 1, client_height + 1),
		)
		.unwrap();

		let msg_conn_ack = MsgConnectionOpenAck::try_from(get_dummy_raw_msg_conn_open_ack(
			client_height,
			client_height,
		))
		.unwrap();

		//
		// Channel handshake messages.
		//
		let msg_chan_init =
			MsgChannelOpenInit::try_from(get_dummy_raw_msg_chan_open_init()).unwrap();

		// The handler will fail to process this b/c the associated connection does not exist
		let mut incorrect_msg_chan_init = msg_chan_init.clone();
		incorrect_msg_chan_init.channel.connection_hops = vec![ConnectionId::new(590)];

		let msg_chan_try =
			MsgChannelOpenTry::try_from(get_dummy_raw_msg_chan_open_try(client_height)).unwrap();

		let msg_chan_ack =
			MsgChannelOpenAck::try_from(get_dummy_raw_msg_chan_open_ack(client_height)).unwrap();

		let msg_chan_close_init =
			MsgChannelCloseInit::try_from(get_dummy_raw_msg_chan_close_init()).unwrap();

		let msg_chan_close_confirm =
			MsgChannelCloseConfirm::try_from(get_dummy_raw_msg_chan_close_confirm(client_height))
				.unwrap();

		let msg_transfer = get_dummy_msg_transfer(35);
		let msg_transfer_two = get_dummy_msg_transfer(36);

		let mut msg_to_on_close =
			MsgTimeoutOnClose::try_from(get_dummy_raw_msg_timeout_on_close(36, 5)).unwrap();
		msg_to_on_close.packet.sequence = 2.into();
		msg_to_on_close.packet.timeout_height = msg_transfer_two.timeout_height;
		msg_to_on_close.packet.timeout_timestamp = msg_transfer_two.timeout_timestamp;

		let denom = msg_transfer_two.token.denom.clone();
		let packet_data = {
			let data = PacketData {
				token: PrefixedCoin { denom, amount: msg_transfer_two.token.amount },
				sender: msg_transfer_two.sender.clone(),
				receiver: msg_transfer_two.receiver.clone(),
				memo: "".to_string(),
			};

			serde_json::to_vec(&data).expect("PacketData's infallible Serialize impl failed")
		};
		msg_to_on_close.packet.data = packet_data;

		let msg_recv_packet = MsgRecvPacket::try_from(get_dummy_raw_msg_recv_packet(35)).unwrap();

		// First, create a client..
		let res = dispatch(
			&mut ctx,
			Ics26Envelope::Ics2Msg(ClientMsg::CreateClient(create_client_msg.clone())),
		);

		assert!(
            res.is_ok(),
            "ICS26 routing dispatch test 'client creation' failed for message {:?} with result: {:?}",
            create_client_msg,
            res
        );

		ctx.scope_port_to_module(msg_chan_init.port_id.clone(), transfer_module_id.clone());

		// Figure out the ID of the client that was just created.
		let mut events = res.unwrap().events;
		let client_id_event = events.pop();
		assert!(client_id_event.is_some(), "There was no event generated for client creation!");
		let client_id = match client_id_event.unwrap() {
			IbcEvent::CreateClient(create_client) => create_client.client_id().clone(),
			event => panic!("unexpected IBC event: {:?}", event),
		};

		let tests: Vec<Test> = vec![
			// Test some ICS2 client functionality.
			Test {
				name: "Client update successful".to_string(),
				msg: Ics26Envelope::Ics2Msg(ClientMsg::UpdateClient(MsgUpdateAnyClient {
					client_id: client_id.clone(),
					client_message: MockHeader::new(update_client_height)
						.with_timestamp(Timestamp::now())
						.into(),
					signer: default_signer.clone(),
				}))
				.into(),
				want_pass: true,
			},
			Test {
				name: "Client update fails due to stale header".to_string(),
				msg: Ics26Envelope::Ics2Msg(ClientMsg::UpdateClient(MsgUpdateAnyClient {
					client_id: client_id.clone(),
					client_message: MockHeader::new(update_client_height).into(),
					signer: default_signer.clone(),
				}))
				.into(),
				want_pass: false,
			},
			Test {
				name: "Connection open init succeeds".to_string(),
				msg: Ics26Envelope::Ics3Msg(ConnectionMsg::ConnectionOpenInit(
					msg_conn_init.with_client_id(client_id.clone()),
				))
				.into(),
				want_pass: true,
			},
			Test {
				name: "Connection open try fails due to InvalidConsensusHeight (too high)"
					.to_string(),
				msg: Ics26Envelope::Ics3Msg(ConnectionMsg::ConnectionOpenTry(Box::new(
					incorrect_msg_conn_try,
				)))
				.into(),
				want_pass: false,
			},
			Test {
				name: "Connection open try succeeds".to_string(),
				msg: Ics26Envelope::Ics3Msg(ConnectionMsg::ConnectionOpenTry(Box::new(
					correct_msg_conn_try.with_client_id(client_id.clone()),
				)))
				.into(),
				want_pass: true,
			},
			Test {
				name: "Connection open ack succeeds".to_string(),
				msg: Ics26Envelope::Ics3Msg(ConnectionMsg::ConnectionOpenAck(Box::new(
					msg_conn_ack,
				)))
				.into(),
				want_pass: true,
			},
			// ICS04
			Test {
				name: "Channel open init succeeds".to_string(),
				msg: Ics26Envelope::Ics4ChannelMsg(ChannelMsg::ChannelOpenInit(msg_chan_init))
					.into(),
				want_pass: true,
			},
			Test {
				name: "Channel open init fail due to missing connection".to_string(),
				msg: Ics26Envelope::Ics4ChannelMsg(ChannelMsg::ChannelOpenInit(
					incorrect_msg_chan_init,
				))
				.into(),
				want_pass: false,
			},
			Test {
				name: "Channel open try succeeds".to_string(),
				msg: Ics26Envelope::Ics4ChannelMsg(ChannelMsg::ChannelOpenTry(msg_chan_try)).into(),
				want_pass: true,
			},
			Test {
				name: "Channel open ack succeeds".to_string(),
				msg: Ics26Envelope::Ics4ChannelMsg(ChannelMsg::ChannelOpenAck(msg_chan_ack)).into(),
				want_pass: true,
			},
			Test { name: "Packet send".to_string(), msg: msg_transfer.into(), want_pass: true },
			// The client update is required in this test, because the proof associated with
			// msg_recv_packet has the same height as the packet TO height (see
			// get_dummy_raw_msg_recv_packet)
			Test {
				name: "Client update successful #2".to_string(),
				msg: Ics26Envelope::Ics2Msg(ClientMsg::UpdateClient(MsgUpdateAnyClient {
					client_id: client_id.clone(),
					client_message: MockHeader::new(update_client_height_after_send)
						.with_timestamp(Timestamp::now())
						.into(),
					signer: default_signer.clone(),
				}))
				.into(),
				want_pass: true,
			},
			Test {
				name: "Receive packet".to_string(),
				msg: Ics26Envelope::Ics4PacketMsg(PacketMsg::RecvPacket(msg_recv_packet.clone()))
					.into(),
				want_pass: true,
			},
			Test {
				name: "Re-Receive packet".to_string(),
				msg: Ics26Envelope::Ics4PacketMsg(PacketMsg::RecvPacket(msg_recv_packet)).into(),
				want_pass: true,
			},
			Test { name: "Packet send".to_string(), msg: msg_transfer_two.into(), want_pass: true },
			Test {
				name: "Client update successful".to_string(),
				msg: Ics26Envelope::Ics2Msg(ClientMsg::UpdateClient(MsgUpdateAnyClient {
					client_id: client_id.clone(),
					client_message: MockHeader::new(update_client_height_after_second_send).into(),
					signer: default_signer.clone(),
				}))
				.into(),
				want_pass: true,
			},
			//ICS04-close channel
			Test {
				name: "Channel close init succeeds".to_string(),
				msg: Ics26Envelope::Ics4ChannelMsg(ChannelMsg::ChannelCloseInit(
					msg_chan_close_init,
				))
				.into(),
				want_pass: true,
			},
			Test {
				name: "Channel close confirm fails cause channel is already closed".to_string(),
				msg: Ics26Envelope::Ics4ChannelMsg(ChannelMsg::ChannelCloseConfirm(
					msg_chan_close_confirm,
				))
				.into(),
				want_pass: false,
			},
			//ICS04-to_on_close
			Test {
				name: "Timeout on close".to_string(),
				msg: Ics26Envelope::Ics4PacketMsg(PacketMsg::ToClosePacket(msg_to_on_close)).into(),
				want_pass: true,
			},
			Test {
				name: "Client upgrade successful".to_string(),
				msg: Ics26Envelope::Ics2Msg(ClientMsg::UpgradeClient(MsgUpgradeAnyClient::new(
					client_id.clone(),
					AnyClientState::Mock(MockClientState::new(MockClientMessage::Header(
						MockHeader::new(upgrade_client_height),
					))),
					AnyConsensusState::Mock(MockConsensusState::new(MockHeader::new(
						upgrade_client_height,
					))),
					Vec::new(),
					Vec::new(),
					default_signer.clone(),
				)))
				.into(),
				want_pass: true,
			},
			Test {
				name: "Client upgrade un-successful".to_string(),
				msg: Ics26Envelope::Ics2Msg(ClientMsg::UpgradeClient(MsgUpgradeAnyClient::new(
					client_id,
					AnyClientState::Mock(MockClientState::new(MockClientMessage::Header(
						MockHeader::new(upgrade_client_height_second),
					))),
					AnyConsensusState::Mock(MockConsensusState::new(MockHeader::new(
						upgrade_client_height_second,
					))),
					Vec::new(),
					Vec::new(),
					default_signer,
				)))
				.into(),
				want_pass: false,
			},
		]
		.into_iter()
		.collect();

		for test in tests {
			let res = match test.msg.clone() {
				TestMsg::Ics26(msg) => dispatch(&mut ctx, msg).map(|_| ()),
				TestMsg::Ics20(msg) => {
					let transfer_module =
						ctx.router_mut().get_route_mut(&transfer_module_id).unwrap();
					ics20_deliver(
						transfer_module
							.as_any_mut()
							.downcast_mut::<DummyTransferModule<MockClientTypes>>()
							.unwrap(),
						&mut HandlerOutputBuilder::new(),
						msg,
					)
					.map(|_| ())
					.map_err(Error::ics04_channel)
				},
			};

			assert_eq!(
				test.want_pass,
				res.is_ok(),
				"ICS26 routing dispatch test '{}' failed for message {:?}\nwith result: {:?}",
				test.name,
				test.msg,
				res
			);
		}
	}
}
