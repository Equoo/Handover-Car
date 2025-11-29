#include <Servo.h>

Servo steering;
Servo throttle;

void setup() {
  steering.attach(9);   // servo output #1
  throttle.attach(10);  // ESC output #2
  
  throttle.write(90);    // neutral or throttle value
  delay(1500);
}

void loop() {
  steering.write(90);   // angle or position
  throttle.write(98);    // neutral or throttle value
}

