# Handover: How It Started

*October 25, 2026*

You know the feeling: you're driving an RC car, it goes behind a wall, and the signal is gone. We wondered what would happen if the car used the phone network instead of a radio. You could drive it from anywhere with signal, watching through a little camera on the front.

So we put a 5G modem, a camera and a small computer on an ordinary RC car. Getting the car to move was the easy part. Minimizing the latency took a lot of work.

The project now has a name: Handover. Until now, it was just "the RC car".

## A car with no doorbell (late 2025)

The first problem showed up before anything was even wired up. On the mobile network, nobody from outside can reach the car directly. It’s like a hotel where every room shares the same phone number and there’s no extension system: guests can call out, but an outside caller has no way to reach a specific room.

So we needed a switchboard operator: a server both sides can call, which patches the two lines together. We rented a small server on the internet to act as a meeting point. The car calls it, the driver calls it, and the server passes messages between the two. Within days, a game controller on one side could send its moves to the other.

The modem was the next wall. Getting it online took a long chain of steps: unlock the SIM card, switch the modem to the right mode, give it the operator's settings, then wait for an address. Any step could fail. Keeping the connection up wasn't easy too. So we have done a script that checks the connection every second and redoes the whole setup when it's lost. Not elegant, but it kept the car online (sometimes).

By early December the wheels turned on command. A safety net was there from the start: if orders stop arriving for a fraction of a second, the car cuts the motor and straightens the wheels.

## The first drive

The first real drive happened indoors at 42. With all its boards and cables, the car looked like a homemade bomb on wheels. It was really funny.

And it worked. We had a car that drove, with live video and a game controller. The video delay wasn't that bad, and we could more or less drive it. Honestly, it was much better than we expected.

It was also very shaky. The connection was poor inside the building, and we couldn't go far because we kept fixing things on the spot. A few days later, we took it outside for the first time.

The setup was a nightmare. We had to start every piece by hand, one after the other: the server, the driver's program, a remote login to the car, a rebuild, then the car's program. Every change meant doing it all again. And the remote login kept cutting out at random, often.

Still, it was a lot of fun. Then the project went quiet for three months.

## Seeing through its eyes (spring and summer 2026)

In March we tackled the video, the heart of the whole thing. We wrote our own camera program, with one trick: when the network gets weak, it lowers the picture quality instead of freezing, then raises it again when things get better. The car adapts its video bitrate on the fly to match what the network can handle.

The summer was a hunt for lost milliseconds. Every bit of delay between the camera and the screen means you turn too late. Along the way we had a picture that came out completely green, and tried a well-known streaming method that turned out to add delay. We removed it the same day.

The best surprise came from the camera chip. To smooth out the data it sent, we asked it to send at a perfectly steady rate. It said yes, confirmed the setting, then refused to start every single time. After two careful tests, we had to admit it: the option was listed, but it didn't actually work.

We got around it another way. Full pictures, the heavy ones that clogged the line, are now only sent when the screen actually needs one. The rest of the time, the car only sends what changed.

## Taking the short road (August 2026)

Until then, everything went through our server. Every picture and every turn of the wheel took a detour that added 15 to 30 thousandths of a second each way.

Now the car and the driver still meet at the server, but they also look for a direct road to each other. As soon as one answers, they switch to it. If it drops, say when the car moves between two phone masts, they're back on the server in under half a second, and the video doesn't even restart.

Writing our own modem manager turned out to be a lot of work for a connection that still kept dropping. In the end, we went back to the standard tools: NetworkManager and ModemManager.

## Taking it outside (August 2026)

In August, we took Handover out for real tests, several times. The street taught us a lot.

When the car stands still, the video is very good. The delay is fine, even when the camera sees a lot of movement.

As soon as it starts rolling, the network gets worse, and the video with it. On top of that, the camera shakes with every bump, since nothing steadies it. Driving gets hard, especially if you want to reach the car's top speed: about 50 km/h, even with all the extra gear.

The hardest part is probably the build itself. It's a small RC car with a 3D-printed lid. On top sit the Raspberry Pi and its 5G board, already fairly heavy. Then an Arduino placed wherever it fits, cables everywhere, and a 600-gram phone power bank with two USB cables dangling to feed the Pi and the modem.

The trickiest of all are the antennas. There are four flat stick-on ones, and finding a good spot for each is a puzzle. Nothing fits together, and everything takes up room.

## The real start

So that's where we are. The car streams video and sound over 5G, follows a game controller and finds the fastest network path on its own. But it does all that on a body that was never meant to carry it.

The real project starts now. First, a proper build, where every part and every antenna has its place and the camera stops shaking. After that, it's time to see how far the phone network can take it.
