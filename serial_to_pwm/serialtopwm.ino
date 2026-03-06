#include <Servo.h>

Servo steering;
Servo throttle;

// microsecond pulse values (typical)
const int STEERING_NEUTRAL_US = 1500;
const int THROTTLE_NEUTRAL_US = 1500;

unsigned long lastUpdate = 0;
const unsigned long TIMEOUT_MS = 100; // 1s -> reset to neutral

// current commanded pulses
volatile int steering_us = STEERING_NEUTRAL_US;
volatile int throttle_us = THROTTLE_NEUTRAL_US;

// serial parser buffer
char buf[64];
uint8_t buf_i = 0;

void setup() {
  Serial.begin(115200);
  steering.attach(9);
  throttle.attach(10);

  steering.writeMicroseconds(STEERING_NEUTRAL_US);
  throttle.writeMicroseconds(THROTTLE_NEUTRAL_US);

  lastUpdate = millis();
}

void loop() {
  // Non-blocking serial read into buffer; expect "105 98\n" or "1500 1480\n"
  while (Serial.available()) {
    char c = (char)Serial.read();
    if (c == '\n') {
      buf[buf_i] = '\0';
      parseLine(buf);
      buf_i = 0;
    } else if (c >= 32 && buf_i < sizeof(buf) - 1) {
      buf[buf_i++] = c;
    } else {
      // ignore weird chars; also avoid overflow
    }
  }

  // Timeout: reset values if no recent data
  if (millis() - lastUpdate > TIMEOUT_MS) {
    steering_us = STEERING_NEUTRAL_US;
    throttle_us = THROTTLE_NEUTRAL_US;
  }

  // Continuously write pulses so servo receives steady PWM
  steering.write(steering_us);
  throttle.write(throttle_us);

  delay(20); // ~50 Hz refresh
}

void parseLine(const char *line) {
  // parse "<a> <b>" where a/b may be degrees (0-180) or microseconds (1000-2200)
  int a = -1, b = -1;
  // sscanf returns number of parsed ints
  int n = sscanf(line, "%d %d", &a, &b);
  if (n >= 1) {
	steering_us = a;
	throttle_us = b;
//    if (a >= 1000 && a <= 2200) steering_us = a;
//    else if (a >= 0 && a <= 180) steering_us = map(a, 0, 180, 1000, 2000);
//
//    if (n >= 2) {
//      if (b >= 1000 && b <= 2200) throttle_us = b;
//      else if (b >= 0 && b <= 180) throttle_us = map(b, 0, 180, 1000, 2000);
//    }
//
    lastUpdate = millis();
  }
  // otherwise ignore malformed line
}

