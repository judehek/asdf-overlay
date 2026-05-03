// Run from open-source/asdf-overlay/ workspace so we exercise the DLL's
// exact asdf-overlay-common build path.
use asdf_overlay_common::{
    ipc::ClientRequest,
    request::{
        BlockCursorInOverlay, BlockInput, ListenInput, Request, SetBlockingCursor, WindowRequest,
    },
};
use bincode::config::standard;

fn encode_decode(label: &str, req: WindowRequest) {
    let pkt = ClientRequest { id: 3, req: Request::Window { id: 329610, request: req } };
    let bytes = bincode::encode_to_vec(&pkt, standard()).expect("encode");
    print!("{label:>28}  bytes=");
    for b in &bytes {
        print!("{:02x} ", b);
    }
    match bincode::decode_from_slice::<ClientRequest, _>(&bytes, standard()) {
        Ok((v, n)) => println!("  decode OK n={n} req={:?}", v.req),
        Err(e) => println!("  decode FAILED {e:?}"),
    }
}

fn main() {
    encode_decode(
        "ListenInput",
        WindowRequest::ListenInput(ListenInput { cursor: true, keyboard: false }),
    );
    encode_decode(
        "BlockInput",
        WindowRequest::BlockInput(BlockInput { block: true }),
    );
    encode_decode(
        "BlockCursorInOverlay",
        WindowRequest::BlockCursorInOverlay(BlockCursorInOverlay { enabled: true }),
    );
    encode_decode(
        "SetBlockingCursor",
        WindowRequest::SetBlockingCursor(SetBlockingCursor { cursor: None }),
    );
}
