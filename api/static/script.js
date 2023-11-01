// Send text field as json POST to server
// Receive json response from server
// using fetch API

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