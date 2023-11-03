// Send text field as json POST to server
// Receive json response from server
// using fetch API

//spotState = new SpotApi();

function sendText() {
    var text = document.getElementById("text").value;
    
    fetch('/', {
        method: 'POST',
        headers: {
            'Content-Type': 'application/json'
        },
        body: text
    })
    .then(res => res.text())
    .then(data => {
        console.log(data);
        document.getElementById("response").innerHTML = data;
    })
    .catch(err => console.log(err));
}

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

class SpotApi {
    constructor() {
        this.PlayerState = defaultState;
        this.lastRequest = {id: -1, jsonrpc: 2.0, method: null, params: null};
        this.socket = new WebSocket("ws://" + location.host + "/");
        this.socket.onmessage = this.handleMessage.bind(this);
        this.socket.onopen = this.handleOpen.bind(this);
    }

    handleOpen() {
        console.log("Socket opened");
        this.sendGetStatus();
    }

    handleMessage(message) {
        let response = JSON.parse(message.data);
        console.log(response);

        // Handle response
        let notification = true; 
        switch (response.method) {
            case "OnNewTrack":
                this.PlayerState.track = response.params;
                break;
            case "OnVolumeChanged":
                this.PlayerState.volume = response.params;
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
                        this.PlayerState.volume = response.result;
                        break;
                    case "getPlayState":
                        this.PlayerState.playing = response.result;
                        break;
                    default:
                        break;
                }
            }
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

}