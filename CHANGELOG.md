# Changelog

## Unreleased Changes

## v0.4.0

* Add very basic SPI interface support to neotron-bmc-pico
* No changes to neotron-bmc-nucleo (it's now out of date)
* Added `neotron-bmc-protocol` crate at v0.1.0

## v0.3.1
* Reset button triggers 250ms low pulse
* Fix STM32F030 support and remove STM32F031 support for neotron-bmc-pico

## v0.3.0
* Add STM32F030 support to neotron-bmc-pico

## v0.2.0
* Change to blink power LED when in standby
* Actually controls DC power and reset (but doesn't check the voltage rails yet)

## v0.1.0
* Skeleton application using knurling template
* Started work on command protocol definition
* LED Blinking Modes defined
* SPI Frame Format revised
