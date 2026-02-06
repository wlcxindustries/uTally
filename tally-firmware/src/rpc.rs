use postcard_rpc::{
    define_dispatch,
    header::{VarHeader, VarKeyKind},
    server::{
        Dispatch, Server, WireTx,
        impls::embassy_net_tcp::dispatch_impl::{
            PacketBuffers, WireRxBuf, WireRxImpl, WireSpawnImpl, WireStorage, WireTxImpl, spawn_fn,
        },
    },
};
use tally_rpc::rpc::{ENDPOINTS_LIST, InfoEndpoint, InfoResponse, TOPICS_IN_LIST, TOPICS_OUT_LIST};

// postcard-rpc stuff
// We have TCP RPC server for device configuration/monitoring

type AppTx = WireTxImpl<NoopRawMutex>;
type AppRx = WireRxImpl;
type AppServer = Server<AppTx, AppRx, WireRxBuf, TallyApp>;
type AppStorage = WireStorage<NoopRawMutex>;
pub struct Context;

define_dispatch! {
    app: TallyApp;
    spawn_fn: spawn_fn;
    tx_impl: AppTx;
    spawn_impl: WireSpawnImpl;
    context: Context;

    endpoints: {
        list: ENDPOINTS_LIST;

        | EndpointTy    | kind | handler |
        | ------------- | ---- | ------- |
        | InfoEndpoint  | async | info_handler |

    };

    topics_in: {
        list: TOPICS_IN_LIST;

        | TopicTy | kind | handler |
        | ------- | ---- | ------- |
    };

    topics_out: {
        list: TOPICS_OUT_LIST;
    };
}

static BUFS: ConstStaticCell<PacketBuffers<1024, 1024>> =
    ConstStaticCell::new(PacketBuffers::new());
static TCP_BUFS: ConstStaticCell<PacketBuffers<1024, 1024>> =
    ConstStaticCell::new(PacketBuffers::new());
static STORAGE: AppStorage = AppStorage::new();
static RPC_SOCK: StaticCell<TcpSocket> = StaticCell::new();

async fn info_handler(_context: &mut Context, _header: VarHeader, _req: ()) -> InfoResponse {
    InfoResponse {
        mac: [0; 6],
        fw_version: (0, 0, 0),
    }
}

pub async fn run_rpc() {
    let tcp_bufs = TCP_BUFS.take();
    let rpc_sock = RPC_SOCK.init(TcpSocket::new(
        eth_stack,
        tcp_bufs.rx_buf.as_mut_slice(),
        tcp_bufs.tx_buf.as_mut_slice(),
    ));

    let bufs = BUFS.take();
    let (tx_impl, rx_impl) = STORAGE
        .accept(
            rpc_sock,
            IpListenEndpoint::from(1234),
            bufs.tx_buf.as_mut_slice(),
        )
        .await;

    let context = Context {};
    let dispatcher = TallyApp::new(context, spawner.into());
    let vkk = dispatcher.min_key_len();
    let mut server: AppServer = Server::new(
        tx_impl,
        rx_impl,
        bufs.rx_buf.as_mut_slice(),
        dispatcher,
        vkk,
    );
    server.run().await;
}
