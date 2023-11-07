const defaultState = {
    track: {
        track_id: "",
        name: "",
        covers: [],
        album: "",
        artists: [],
        show_name: ""
    },
    playing: "Stopped",
    volume: 0
};

const defaultRequest = {
    id: -1,
    jsonrpc: 2.0,
    method: null,
    params: null
};

class LibrespotApi {
    constructor() {
        this.PlayerState = defaultState;
        this.lastRequest = defaultRequest;
        this.socket = new WebSocket("ws://" + location.host + "/");
        this.socket.onmessage = this.handleMessage.bind(this);
        this.socket.onopen = this.handleOpen.bind(this);
        this.socket.onclose = this.handleClose.bind(this);
        this.onupdate = null;
    }

    handleOpen() {
        console.log("Socket opened");
        this.sendGetStatus();
    }

    handleClose() {
        console.log("Socket closed");
        this.PlayerState = defaultState;
        this.lastRequest = defaultRequest;
        if (this.onupdate != null) {
            this.onupdate(this.PlayerState);
        }
    }

    handleMessage(message) {
        let response = JSON.parse(message.data);
        console.log(response);

        // Handle response
        let notification = true; 
        switch (response.method) {
            case "OnNewTrack":
                this.PlayerState.track = response.params.track;
                break;
            case "OnVolumeChange":
                this.PlayerState.volume = response.params.volume;
                break;
            case "OnPlay":
                this.PlayerState.playing = "Playing";
                break;
            case "OnPause":
                this.PlayerState.playing = "Paused";
                break;
            case "OnStop":
                this.PlayerState = defaultState;
                break;
            case "OnShuffleChange":
                this.PlayerState.shuffle = response.params.shuffle;
                break;
            case undefined:
                notification = false;
                break;
            default:
                break;
        }

        if (!notification) {
            // Handle response to request
            if(response.id == this.lastRequest.id && response.error == undefined) {
                switch (this.lastRequest.method) {
                    case "getStatus":
                        this.PlayerState = response.result;
                        break;
                    case "getVolume":
                        this.PlayerState.volume = response.result.volume;
                        break;
                    case "getPlayState":
                        this.PlayerState.playing = response.result.playing;
                        break;
                    default:
                        break;
                }
            }
        }

        // Update UI
        if (this.onupdate != null) {
            this.onupdate(this.PlayerState);
        }
    }

    sendGetStatus() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "getStatus"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendSetPlay() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "setPlay"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendSetPause() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "setPause"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendSetVolume(volume) {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "setVolume",
            params: volume
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendGetVolume() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "getVolume"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendGetPlayState() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "getPlayState"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendSetNext() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "setNext"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendSetShuffleOn() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "setShuffleOn"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

    sendSetShuffleOff() {
        let req = {
            id: this.lastRequest.id + 1,
            jsonrpc: 2.0,
            method: "setShuffleOff"
        };
        this.lastRequest = req;
        this.socket.send(JSON.stringify(req));
    }

}