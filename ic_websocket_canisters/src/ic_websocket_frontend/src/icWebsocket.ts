import {
  ActorSubclass,
  Cbor,
  Certificate,
  compare,
  HashTree,
  HttpAgent,
  lookup_path,
  reconstruct,
} from "@dfinity/agent";
import { Principal } from "@dfinity/principal";

import * as ed from '@noble/ed25519';

const CLIENT_SECRET_KEY_STORAGE_KEY = "ic_websocket_client_secret_key";

type WsMessage = {
  key: string;
  cert: ArrayBuffer;
  tree: ArrayBuffer;
  val: ArrayBuffer;
}

type WsMessageContent = {
  sequence_num: number;
  timestamp: number;
  message: ArrayBuffer;
};

type IcWebSocketConfig = {
  /**
   * The canister id of the canister to open the websocket to.
   */
  canisterId: string;
  /**
   * The canister actor class.
   * 
   * It must implement the methods:
   * - `ws_register`
   * - `ws_message`
   */
  canisterActor: ActorSubclass<any>
  /**
   * The IC network url to use for the HttpAgent. It can be a local replica (e.g. http://localhost:4943) or the IC mainnet (https://ic0.io).
   */
  networkUrl: string;
  /**
   * If `true`, it means that the network is a local replica and the HttpAgent will fetch the root key.
   */
  localTest: boolean;
  /**
   * If `true`, the secret key will be stored in local storage and reused on subsequent page loads.
   */
  persistKey?: boolean;
};

type WsParameters = ConstructorParameters<typeof WebSocket>;

export default class IcWebSocket {
  readonly canisterId: Principal;
  readonly agent: HttpAgent;
  readonly canisterActor: ActorSubclass<any>;
  private wsInstance: WebSocket;
  private secretKey: Uint8Array | string;
  private nextReceivedNum: number;
  private sequenceNum = 0;

  onclose: ((this: IcWebSocket, ev: CloseEvent) => any) | null = null;
  onerror: ((this: IcWebSocket, ev: Event) => any) | null = null;
  onmessage: ((this: IcWebSocket, ev: MessageEvent<string>) => any) | null = null;
  onopen: ((this: IcWebSocket, ev: Event) => any) | null = null;

  /**
   * Creates a new IcWebSocket instance.
   * @param url The gateway address.
   * @param protocols The protocols to use in the WebSocket.
   * @param config The IcWebSocket configuration.
   */
  constructor(url: WsParameters[0], protocols: WsParameters[1], config: IcWebSocketConfig) {
    this.canisterId = Principal.fromText(config.canisterId);

    if (!config.canisterActor.ws_register) {
      throw new Error("Canister actor does not implement the ws_register method");
    }

    if (!config.canisterActor.ws_message) {
      throw new Error("Canister actor does not implement the ws_message method");
    }

    if (config.persistKey) {
      // attempt to load the secret key from local storage (stored in hex format)
      const storedKey = localStorage.getItem(CLIENT_SECRET_KEY_STORAGE_KEY);

      if (storedKey) {
        console.log("Using stored key");
        this.secretKey = storedKey;
      } else {
        console.log("Generating and storing new key");
        this.secretKey = ed.utils.randomPrivateKey(); // Generate new key for this websocket connection.
        localStorage.setItem(CLIENT_SECRET_KEY_STORAGE_KEY, ed.etc.bytesToHex(this.secretKey));
      }
    } else {
      console.log("Generating new key");
      this.secretKey = ed.utils.randomPrivateKey(); // Generate new key for this websocket connection.
    }

    this.canisterActor = config.canisterActor;

    this.nextReceivedNum = 0; // Received signed messages need to come in the correct order, with sequence numbers 0, 1, 2...
    // TODO: IcWebSocket should accept parameters in the config object.
    this.wsInstance = new WebSocket(url, protocols); // Gateway address. Here localhost to reproduce the demo.
    this.wsInstance.binaryType = "arraybuffer";
    this._bindWsEvents();

    this.agent = new HttpAgent({ host: config.networkUrl });
    if (config.localTest) {
      this.agent.fetchRootKey();
    }
  }

  private async _makeMessage(data: any) {
    // Our demo application uses simple text message.
    const content = Cbor.encode(data);

    // Message with all required fields.
    const websocketMessage = Cbor.encode({
      client_key: await ed.getPublicKeyAsync(this.secretKey), // public key generated by the client
      sequence_num: this.sequenceNum, // Next sequence number to ensure correct order.
      timestamp: Date.now() * 1000000,
      message: content, // Binary application message.
    });

    // Sign the message
    const toSign = new Uint8Array(websocketMessage);
    const sig = await ed.signAsync(toSign, this.secretKey);

    // Final signed websocket message
    const message = {
      content: toSign,
      sig: sig,
    };

    // Send GatewayMessage variant
    return {
      RelayedFromClient: message,
    };
  }

  async send(data: any) {
    // we send the message directly to the canister, not to the gateway
    const message = await this._makeMessage(data);
    try {
      const sendResult = await this.canisterActor.ws_message(message);

      if ("Err" in sendResult) {
        throw new Error(sendResult.Err);
      }
    } catch (error) {
      console.error("[send] Error:", error);
      throw error;
    }
    this.sequenceNum += 1;
  }

  close() {
    this.wsInstance.close();
  }

  private _bindWsEvents() {
    this.wsInstance.onopen = this._onWsOpen.bind(this);
    this.wsInstance.onmessage = this._onWsMessage.bind(this);
    this.wsInstance.onclose = this._onWsClose.bind(this);
    this.wsInstance.onerror = this._onWsError.bind(this);
  }

  private async _onWsOpen() {
    console.log("[open] WS opened");
    const publicKey = await ed.getPublicKeyAsync(this.secretKey);
    // Put the public key in the canister
    await this.canisterActor.ws_register(publicKey);
    this.sequenceNum = 0;

    // Send the first message with client and canister id
    const cborContent = Cbor.encode({
      client_key: publicKey,
      canister_id: this.canisterId,
    });

    // Sign so that the gateway can verify canister and client ids match
    const toSign = new Uint8Array(cborContent);
    const sig = await ed.signAsync(toSign, this.secretKey);

    const message = {
      content: cborContent,
      sig: sig,
    };

    // Send the first message
    const wsMessage = Cbor.encode(message);
    this.wsInstance.send(wsMessage);
    this.sequenceNum = 0;

    console.log("[open] Sent first message");

    // the onopen callback is called when the first confirmation message is received from the canister
    // see _onWsMessage function
  }

  private async _onWsMessage(event: MessageEvent<ArrayBuffer>) {
    if (this.nextReceivedNum == 0) {
      // first received message
      console.log('[message]: first message', event.data);
      this.nextReceivedNum += 1;

      console.log("[open] Connection opened");

      if (this.onopen) {
        this.onopen.call(this, new Event("open"));
      }
    } else {
      const res = Cbor.decode<WsMessage>(event.data);

      let key, val, cert, tree;
      key = res.key;
      val = new Uint8Array(res.val);
      cert = res.cert;
      tree = res.tree;
      const websocketMsg = Cbor.decode<WsMessageContent>(val);

      // Check the sequence number
      const receivedNum = websocketMsg.sequence_num;
      if (receivedNum != this.nextReceivedNum) {
        console.log(`Received message sequence number (${receivedNum}) does not match next expected value (${this.nextReceivedNum}). Message ignored.`);
        return;
      }
      this.nextReceivedNum += 1;

      // Inspect the timestamp
      const time = websocketMsg.timestamp;
      const delaySeconds = (Date.now() * (10 ** 6) - time) / (10 ** 9);
      console.log(`(time now) - (message timestamp) = ${delaySeconds}s`);

      // Verify the certificate (canister signature)
      const valid = await validateWsMessageBody(this.canisterId, key, val, cert, tree, this.agent);
      console.log(`Certificate validation: ${valid}`);
      if (!valid) {
        console.log(`Message ignored.`);
        return;
      }

      // Message has been verified
      const appMsg = Cbor.decode<{ text: string }>(websocketMsg.message);
      const text = appMsg.text;
      console.log(`[message] Message from canister: ${text}`);

      if (this.onmessage) {
        this.onmessage.call(this, new MessageEvent("message", {
          data: text,
        }));
      }
    }
  }

  private _onWsClose(event: CloseEvent) {
    if (event.wasClean) {
      console.log(
        `[close] Connection closed, code=${event.code} reason=${event.reason}`
      );
    } else {
      console.log("[close] Connection died");
    }

    if (this.onclose) {
      this.onclose.call(this, event);
    }
  }

  private _onWsError(error: Event) {
    console.log(`[error]`, error);

    if (this.onerror) {
      this.onerror.call(this, error);
    }
  }
}

function equal(buf1: ArrayBuffer, buf2: ArrayBuffer) {
  return compare(buf1, buf2) === 0;
}

async function validateWsMessageBody(
  canisterId: Principal,
  path: string,
  body: Uint8Array,
  certificate: ArrayBuffer,
  tree: ArrayBuffer,
  agent: HttpAgent,
) {
  let cert;
  try {
    cert = await Certificate.create({
      certificate,
      canisterId,
      rootKey: agent.rootKey!
    });
  } catch (error) {
    return false;
  }

  const hashTree = Cbor.decode<HashTree>(tree);
  const reconstructed = await reconstruct(hashTree);
  const witness = cert.lookup([
    "canister",
    canisterId.toUint8Array(),
    "certified_data"
  ]);

  if (!witness) {
    throw new Error(
      "Could not find certified data for this canister in the certificate."
    );
  }

  // First validate that the Tree is as good as the certification.
  if (!equal(witness, reconstructed)) {
    console.error("Witness != Tree passed in ic-certification");
    return false;
  }

  // Next, calculate the SHA of the content.
  const sha = await crypto.subtle.digest("SHA-256", body);
  let treeSha = lookup_path(["websocket", path], hashTree);

  if (!treeSha) {
    // Allow fallback to `index.html`.
    treeSha = lookup_path(["websocket"], hashTree);
  }

  if (!treeSha) {
    // The tree returned in the certification header is wrong. Return false.
    // We don't throw here, just invalidate the request.
    console.error(
      `Invalid Tree in the header. Does not contain path ${JSON.stringify(
        path
      )}`
    );
    return false;
  }

  return !!treeSha && equal(sha, treeSha);
}