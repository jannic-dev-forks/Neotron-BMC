//! Neotron BMC Firmware
//!
//! This is the firmware for the Neotron Board Management Controller (BMC) as
//! fitted to a Neotron Pico. It controls the power, reset, UART and PS/2 ports
//! on that Neotron mainboard. For more details, see the `README.md` file.
//!
//! # Licence
//! This source code as a whole is licensed under the GPL v3. Third-party crates
//! are covered by their respective licences.

#![no_main]
#![no_std]

use core::convert::TryFrom;

use heapless::spsc::{Consumer, Producer, Queue};
use rtic::app;
use stm32f0xx_hal::{
	gpio::gpioa::{PA10, PA11, PA12, PA15, PA2, PA3, PA4, PA8, PA9},
	gpio::gpiob::{PB0, PB3, PB4, PB5},
	gpio::gpiof::{PF0, PF1},
	gpio::{Alternate, Floating, Input, Output, PullDown, PullUp, PushPull, AF1},
	pac,
	prelude::*,
	rcc, serial,
};

use neotron_bmc_commands::Command;
use neotron_bmc_pico::{self as _, speaker};
use neotron_bmc_protocol as proto;

/// Version string auto-generated by git.
static VERSION: [u8; 32] = *include_bytes!(concat!(env!("OUT_DIR"), "/version.txt"));

/// At what rate do we blink the status LED when we're running?
const LED_PERIOD_MS: u64 = 1000;

/// How often we poll the power and reset buttons in milliseconds.
const DEBOUNCE_POLL_INTERVAL_MS: u64 = 75;

/// Length of a reset pulse, in milliseconds
const RESET_DURATION_MS: u64 = 250;

/// The states we can be in controlling the DC power
#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum DcPowerState {
	/// We've just enabled the DC power (so ignore any incoming long presses!)
	Starting = 1,
	/// We are now fully on. Look for a long press to turn off.
	On = 2,
	/// We are fully off.
	Off = 0,
}

/// This is our system state, as accessible via SPI reads and writes.
#[derive(Debug, Default)]
pub struct RegisterState {
	/// The version of this firmware
	firmware_version: [u8; 32],
	/// Bytes we've read from the keyboard, ready for sending to the host
	ps2_kb_bytes: heapless::Deque<u8, 16>,
	/// Used for holding our TX buffer, so we can re-send if required
	scratch: [u8; 16],
	/// A copy of the last request, so we can spot duplicates and re-send
	/// without re-doing a FIFO read. This happens if our response gets a CRC
	/// error.
	last_req: Option<proto::Request>,
	/// The config of the speaker
	speaker: speaker::RegisterState,
}

#[app(device = crate::pac, peripherals = true, dispatchers = [USB, USART3_4_5_6, TIM14, TIM15, TIM16, TIM17, PVD])]
mod app {
	use super::*;
	use systick_monotonic::*; // Implements the `Monotonic` trait

	pub enum Message {
		/// Word from PS/2 port 0
		Ps2Data0(u16),
		/// Word from PS/2 port 1
		Ps2Data1(u16),
		/// SPI driver has a Request for us
		SpiRx,
		/// SPI CS went low (active)
		SpiEnable,
		/// SPI CS went high (inactive)
		SpiDisable,
		/// The power button was given a press
		PowerButtonShortPress,
		/// The power button was held down
		PowerButtonLongPress,
		/// The power button was released
		PowerButtonRelease,
		/// The reset button was given a tap
		ResetButtonShortPress,
		/// The UART got some data
		UartByte(u8),
		/// The speaker's config should be reset
		SpeakerDisable,
	}

	#[shared]
	struct Shared {
		/// The power LED (D1101)
		#[lock_free]
		led_power: PB0<Output<PushPull>>,
		/// The speaker (J1006)
		speaker: speaker::Hardware,
		/// The FTDI UART header (J105)
		#[lock_free]
		serial: serial::Serial<pac::USART1, PA9<Alternate<AF1>>, PA10<Alternate<AF1>>>,
		/// The Clear-To-Send line on the FTDI UART header (which the serial object can't handle)
		#[lock_free]
		_pin_uart_cts: PA11<Alternate<AF1>>,
		/// The Ready-To-Receive line on the FTDI UART header (which the serial object can't handle)
		#[lock_free]
		_pin_uart_rts: PA12<Alternate<AF1>>,
		/// The power button
		#[lock_free]
		button_power: PF0<Input<PullUp>>,
		/// The reset button
		#[lock_free]
		button_reset: PF1<Input<PullUp>>,
		/// Tracks DC power state
		state_dc_power_enabled: DcPowerState,
		/// Controls the DC-DC PSU
		#[lock_free]
		pin_dc_on: PA3<Output<PushPull>>,
		/// Controls the Reset signal across the main board, putting all the
		/// chips (except this BMC!) in reset when pulled low.
		pin_sys_reset: PA2<Output<PushPull>>,
		/// Clock pin for PS/2 Keyboard port
		#[lock_free]
		ps2_clk0: PA15<Input<Floating>>,
		/// Clock pin for PS/2 Mouse port
		#[lock_free]
		_ps2_clk1: PB3<Input<Floating>>,
		/// Data pin for PS/2 Keyboard port
		#[lock_free]
		ps2_dat0: PB4<Input<Floating>>,
		/// Data pin for PS/2 Mouse port
		#[lock_free]
		_ps2_dat1: PB5<Input<Floating>>,
		/// The external interrupt peripheral
		#[lock_free]
		exti: pac::EXTI,
		/// Read messages here
		#[lock_free]
		msg_q_out: Consumer<'static, Message, 8>,
		/// Write messages here
		msg_q_in: Producer<'static, Message, 8>,
		/// SPI Peripheral
		spi: neotron_bmc_pico::spi::SpiPeripheral<5, 64>,
		/// CS pin
		pin_cs: PA4<Input<PullDown>>,
		/// Keyboard PS/2 decoder
		kb_decoder: neotron_bmc_pico::ps2::Ps2Decoder,
	}

	#[local]
	struct Local {
		/// Tracks power button state for short presses. 75ms x 2 = 150ms is a short press
		press_button_power_short: debouncr::Debouncer<u8, debouncr::Repeat2>,
		/// Tracks power button state for long presses. 75ms x 16 = 1200ms is a long press
		press_button_power_long: debouncr::Debouncer<u16, debouncr::Repeat16>,
		/// Tracks reset button state for short presses. 75ms x 2 = 150ms is a long press
		press_button_reset_short: debouncr::Debouncer<u8, debouncr::Repeat2>,
		/// Run-time Clock Control (required for resetting peripheral blocks)
		rcc: Option<rcc::Rcc>,
		/// IRQ pin
		pin_irq: PA8<Output<PushPull>>,
	}

	#[monotonic(binds = SysTick, default = true)]
	type MyMono = Systick<200>; // 200 Hz (= 5ms) timer tick

	/// The entry point to our application.
	///
	/// Sets up the hardware and spawns the regular tasks.
	///
	/// * Task `led_power_blink` - blinks the LED
	/// * Task `button_poll` - checks the power and reset buttons
	#[init(local = [ queue: Queue<Message, 8> = Queue::new()])]
	fn init(ctx: init::Context) -> (Shared, Local, init::Monotonics) {
		defmt::info!(
			"Neotron BMC version {=[u8]:a} booting",
			VERSION.split(|c| *c == 0).next().unwrap()
		);

		let dp: pac::Peripherals = ctx.device;
		let cp: cortex_m::Peripherals = ctx.core;

		let mut flash = dp.FLASH;
		let mut rcc = dp
			.RCC
			.configure()
			.hclk(48.mhz())
			.pclk(48.mhz())
			.sysclk(48.mhz())
			.freeze(&mut flash);

		defmt::info!("Configuring SysTick...");
		// Initialize the monotonic timer using the Cortex-M SysTick peripheral
		let mono = Systick::new(cp.SYST, rcc.clocks.sysclk().0);

		defmt::info!("Creating pins...");
		let gpioa = dp.GPIOA.split(&mut rcc);
		let gpiob = dp.GPIOB.split(&mut rcc);
		let gpiof = dp.GPIOF.split(&mut rcc);
		// We have to have the closure return a tuple of all our configured
		// pins because by taking fields from `gpioa`, `gpiob`, etc, we leave
		// them as partial structures. This prevents us from having a call to
		// `disable_interrupts` for each pin. We can't simply do the `let foo
		// = ` inside the closure either, as the pins would be dropped when
		// the closure ended. So, we have this slightly awkward syntax
		// instead. Do ensure the pins and the variables line-up correctly;
		// order is important!
		let (
			uart_tx,
			uart_rx,
			_pin_uart_cts,
			_pin_uart_rts,
			mut led_power,
			_speaker_pwm,
			button_power,
			button_reset,
			mut pin_dc_on,
			mut pin_sys_reset,
			ps2_clk0,
			_ps2_clk1,
			ps2_dat0,
			_ps2_dat1,
			pin_cs,
			pin_sck,
			pin_cipo,
			pin_copi,
			mut pin_irq,
		) = cortex_m::interrupt::free(|cs| {
			(
				// uart_tx,
				gpioa.pa9.into_alternate_af1(cs),
				// uart_rx,
				gpioa.pa10.into_alternate_af1(cs),
				// _pin_uart_cts,
				gpioa.pa11.into_alternate_af1(cs),
				// _pin_uart_rts,
				gpioa.pa12.into_alternate_af1(cs),
				// led_power,
				gpiob.pb0.into_push_pull_output(cs),
				// speaker_pwm,
				gpiob.pb1.into_alternate_af0(cs),
				// button_power,
				gpiof.pf0.into_pull_up_input(cs),
				// button_reset,
				gpiof.pf1.into_pull_up_input(cs),
				// pin_dc_on,
				gpioa.pa3.into_push_pull_output(cs),
				// pin_sys_reset,
				gpioa.pa2.into_push_pull_output(cs),
				// ps2_clk0,
				gpioa.pa15.into_floating_input(cs),
				// _ps2_clk1,
				gpiob.pb3.into_floating_input(cs),
				// ps2_dat0,
				gpiob.pb4.into_floating_input(cs),
				// _ps2_dat1,
				gpiob.pb5.into_floating_input(cs),
				// pin_cs,
				gpioa.pa4.into_pull_down_input(cs),
				// pin_sck,
				gpioa.pa5.into_alternate_af0(cs),
				// pin_cipo,
				{
					// Force 'high speed' mode first, then go into AF0 (high
					// speed mode is sticky in this HAL revision).
					let pin = gpioa.pa6.into_push_pull_output_hs(cs);
					pin.into_alternate_af0(cs)
				},
				// pin_copi,
				gpioa.pa7.into_alternate_af0(cs),
				// pin_irq
				gpioa.pa8.into_push_pull_output(cs),
			)
		});

		// Put host in reset
		pin_sys_reset.set_low().unwrap();
		// Turn the PSU off
		pin_dc_on.set_low().unwrap();
		// IRQ is active low; we have no need for service.
		pin_irq.set_high().unwrap();
		// Power LED is off
		led_power.set_low().unwrap();

		defmt::info!("Creating UART...");

		let mut serial =
			serial::Serial::usart1(dp.USART1, (uart_tx, uart_rx), 115_200.bps(), &mut rcc);

		serial.listen(serial::Event::Rxne);

		// Put SPI into Peripheral mode (i.e. CLK is an input) and enable the RX interrupt.
		let spi = neotron_bmc_pico::spi::SpiPeripheral::new(
			dp.SPI1,
			(pin_sck, pin_cipo, pin_copi),
			&mut rcc,
		);

		led_power.set_low().unwrap();

		speaker::RegisterState::default().setup(&mut rcc, &dp.TIM14);

		// Set EXTI15 to use PORT A (PA15) - button input
		dp.SYSCFG.exticr4.modify(|_r, w| w.exti15().pa15());

		// Enable EXTI15 interrupt as external falling edge
		dp.EXTI.imr.modify(|_r, w| w.mr15().set_bit());
		dp.EXTI.emr.modify(|_r, w| w.mr15().set_bit());
		dp.EXTI.ftsr.modify(|_r, w| w.tr15().set_bit());

		// Set EXTI4 to use PORT A (PA4) - SPI CS
		dp.SYSCFG.exticr2.modify(|_r, w| w.exti4().pa4());

		// Enable EXTI4 interrupt as external falling/rising edge
		dp.EXTI.imr.modify(|_r, w| w.mr4().set_bit());
		dp.EXTI.emr.modify(|_r, w| w.mr4().set_bit());
		dp.EXTI.ftsr.modify(|_r, w| w.tr4().set_bit());
		dp.EXTI.rtsr.modify(|_r, w| w.tr4().set_bit());

		// Spawn the tasks that run all the time
		led_power_blink::spawn().unwrap();
		button_poll::spawn().unwrap();

		defmt::info!("Init complete!");

		let (msg_q_in, msg_q_out) = ctx.local.queue.split();

		let shared_resources = Shared {
			serial,
			_pin_uart_cts,
			_pin_uart_rts,
			led_power,
			speaker: speaker::Hardware::new(dp.TIM14),
			button_power,
			button_reset,
			state_dc_power_enabled: DcPowerState::Off,
			pin_dc_on,
			pin_sys_reset,
			ps2_clk0,
			_ps2_clk1,
			ps2_dat0,
			_ps2_dat1,
			exti: dp.EXTI,
			msg_q_out,
			msg_q_in,
			spi,
			pin_cs,
			kb_decoder: neotron_bmc_pico::ps2::Ps2Decoder::new(),
		};
		let local_resources = Local {
			press_button_power_short: debouncr::debounce_2(false),
			press_button_power_long: debouncr::debounce_16(false),
			press_button_reset_short: debouncr::debounce_2(false),
			rcc: Some(rcc),
			pin_irq,
		};
		let init = init::Monotonics(mono);
		(shared_resources, local_resources, init)
	}

	/// Our idle task.
	///
	/// This task is called when there is nothing else to do.
	#[idle(shared = [msg_q_out, msg_q_in, spi, state_dc_power_enabled, pin_dc_on, pin_sys_reset, speaker], local = [pin_irq, rcc, speaker_task_handle: Option<speaker_pwm_stop::MyMono::SpawnHandle> = None])]
	fn idle(mut ctx: idle::Context) -> ! {
		// TODO: Get this from the VERSION static variable or from PKG_VERSION
		let mut register_state = RegisterState {
			firmware_version: VERSION,
			..Default::default()
		};
		// Take this out of the `local` object to avoid sharing issues.
		let mut rcc = ctx.local.rcc.take().unwrap();
		defmt::info!("Idle is running...");
		let mut irq_masked = true;
		let mut is_high = false;
		loop {
			if !irq_masked && !register_state.ps2_kb_bytes.is_empty() {
				// We need service
				ctx.local.pin_irq.set_low().unwrap();
				if is_high {
					defmt::trace!("irq set");
					is_high = false;
				}
			} else {
				// We do not need service
				ctx.local.pin_irq.set_high().unwrap();
				if !is_high {
					defmt::trace!("irq clear");
					is_high = true;
				}
			}

			match ctx.shared.msg_q_out.dequeue() {
				Some(Message::Ps2Data0(word)) => {
					if let Some(byte) = neotron_bmc_pico::ps2::Ps2Decoder::check_word(word) {
						defmt::info!("< KB 0x{:x}", byte);
						if let Err(_x) = register_state.ps2_kb_bytes.push_back(byte) {
							defmt::warn!("KB overflow!");
						}
					} else {
						defmt::warn!("< Bad KB 0x{:x}", word);
					}
				}
				Some(Message::Ps2Data1(word)) => {
					if let Some(byte) = neotron_bmc_pico::ps2::Ps2Decoder::check_word(word) {
						defmt::info!("< MS 0x{:x}", byte);
					} else {
						defmt::warn!("< Bad MS 0x{:x}", word);
					}
				}
				Some(Message::PowerButtonLongPress) => {
					if ctx.shared.state_dc_power_enabled.lock(|r| *r) == DcPowerState::On {
						defmt::info!("Power off requested!");
						ctx.shared
							.state_dc_power_enabled
							.lock(|r| *r = DcPowerState::Off);
						// Stop any SPI stuff that's currently going on (the host is about to be powered off)
						ctx.shared.spi.lock(|s| s.reset(&mut rcc));
						// Put the host into reset
						ctx.shared.pin_sys_reset.lock(|pin| pin.set_low().unwrap());
						// Shut off the 5V power
						ctx.shared.pin_dc_on.set_low().unwrap();
						// Mask the IRQ to avoid back-powering the host
						irq_masked = true;
						// Start LED blinking again
						led_power_blink::spawn().unwrap();
					}
				}
				Some(Message::PowerButtonShortPress) => {
					if ctx.shared.state_dc_power_enabled.lock(|r| *r) == DcPowerState::Off {
						defmt::info!("Power up requested!");
						// Button pressed - power on system.
						// Step 1 - enable speaker and play power-up tune
						ctx.shared.speaker.lock(|speaker| speaker.enable());
						speaker_init_tune::spawn().unwrap();
						// Step 2 - Note our new power state
						ctx.shared
							.state_dc_power_enabled
							.lock(|r| *r = DcPowerState::Starting);
						// Step 3 - Hold reset line (active) low
						ctx.shared.pin_sys_reset.lock(|pin| pin.set_low().unwrap());
						// Step 4 - Turn on PSU
						ctx.shared.pin_dc_on.set_high().unwrap();
						// Step 5 - Leave it in reset for a while.
						// TODO: Start monitoring 3.3V and 5.0V rails here
						// TODO: Take system out of reset when 3.3V and 5.0V are good
						// Returns an error if it's already scheduled (but we don't care)
						let _ = exit_reset::spawn_after(RESET_DURATION_MS.millis());
						// Set 6 - unmask the IRQ
						irq_masked = false;
					}
				}
				Some(Message::PowerButtonRelease) => {
					if ctx.shared.state_dc_power_enabled.lock(|r| *r) == DcPowerState::Starting {
						defmt::info!("Power button released.");
						// Button released after power on. Change the power
						// state machine t "On". We were in 'Starting' to ignore
						// any further button events until the button had been
						// released.
						ctx.shared
							.state_dc_power_enabled
							.lock(|r| *r = DcPowerState::On);
					}
				}
				Some(Message::ResetButtonShortPress) => {
					// Is the board powered on? Don't do a reset if it's powered off.
					if ctx.shared.state_dc_power_enabled.lock(|r| *r) == DcPowerState::On {
						defmt::info!("Reset!");
						ctx.shared.pin_sys_reset.lock(|pin| pin.set_low().unwrap());

						// play power-up tune
						ctx.shared.speaker.lock(|speaker| speaker.enable());
						speaker_init_tune::spawn().unwrap();

						ctx.shared.pin_sys_reset.lock(|pin| pin.set_low().unwrap());
						ctx.shared.spi.lock(|s| s.reset(&mut rcc));
						// Step 2 - Hold reset line (active) low
						ctx.shared.pin_sys_reset.lock(|pin| pin.set_low().unwrap());
						// Step 3 - Take it out of reset in a short while
						// Returns an error if it's already scheduled (but we don't care)
						let _ = exit_reset::spawn_after(RESET_DURATION_MS.millis());
					}
				}
				Some(Message::SpiEnable) => {
					if ctx.shared.state_dc_power_enabled.lock(|r| *r) != DcPowerState::Off {
						// Turn on the SPI peripheral and expect four bytes (the
						// length of a Request).
						ctx.shared.spi.lock(|s| s.start(4));
					} else {
						// Ignore message - it'll be the CS line being pulled low when the host is powered off
						defmt::info!("Ignoring spurious CS low");
					}
				}
				Some(Message::SpiDisable) => {
					// Turn off the SPI peripheral. Don't need to check power state for this.
					ctx.shared.spi.lock(|s| s.stop());
					defmt::trace!("SPI Disable");
				}
				Some(Message::SpiRx) => {
					defmt::trace!("SpiRx");
					// Look for something in the SPI bytes received buffer:
					let mut req = None;
					ctx.shared.spi.lock(|spi| {
						if let Some((data, crc)) = spi.get_received() {
							use proto::Receivable;
							match proto::Request::from_bytes_with_crc(data, crc) {
								Ok(inner_req) => {
									defmt::trace!("Got packet");
									req = Some(inner_req);
								}
								Err(proto::Error::BadLength) => {
									// This is a programming bug. We said
									// start(4) earlier, so there should be four
									// bytes here.
									panic!("Wanted 4, got {}", data.len());
								}
								Err(e) => {
									defmt::warn!("Bad Req {:?} ({=[u8]:x}", e, data);
								}
							}
						}
					});

					// If we got a valid message, queue it so we can look at it next time around
					if let Some(req) = req {
						process_command(req, &mut register_state, |rsp| {
							ctx.shared.spi.lock(|spi| {
								spi.set_transmit_sendable(rsp).unwrap();
							});
						});
					}
				}
				Some(Message::UartByte(rx_byte)) => {
					defmt::info!("UART RX {:?}", rx_byte);
					// TODO: Copy byte to software buffer and turn UART RX
					// interrupt off if buffer is full
				}
				Some(Message::SpeakerDisable) => {
					defmt::trace!("Speaker disabled");
					ctx.shared.speaker.lock(|speaker| speaker.disable());
					register_state.speaker.set_duration(0);
				}
				None => {
					// No messages
				}
			}

			// The speaker PWM needs to be updated (register was updated)
			if register_state.speaker.needs_update() {
				defmt::info!("speaker PWM update");
				register_state.speaker.set_needs_update(false);

				let register = &mut register_state.speaker;
				let task_handle = ctx.local.speaker_task_handle.take();

				let keep_playing = ctx.shared.speaker.lock(|speaker| {
					speaker.update(register, || {
						if let Some(h) = task_handle {
							// if there's a running "stop" task, reschedule it
							defmt::trace!("Speaker task cancelled!");
							h.cancel().unwrap_or_default();
						}
					})
				});

				if keep_playing {
					// otherwise, spawn a new one
					defmt::trace!("Speaker task spawned!");
					ctx.local.speaker_task_handle.replace(
						speaker_pwm_stop::spawn_after((register.duration() as u64).millis())
							.unwrap(),
					);
				}
			}
			// TODO: Read ADC for 3.3V and 5.0V rails and check good
		}
	}

	/// This is the external GPIO interrupt task.
	///
	/// It handles PS/2 clock edges, and SPI chip select edges.
	///
	/// It is very high priority, as we can't afford to miss a PS/2 clock edge.
	#[task(
		binds = EXTI4_15,
		priority = 4,
		shared = [ps2_clk0, msg_q_in, ps2_dat0, exti, pin_cs, kb_decoder],
	)]
	fn exti4_15_interrupt(mut ctx: exti4_15_interrupt::Context) {
		let pr = ctx.shared.exti.pr.read();
		// Is this EXT15 (PS/2 Port 0 clock input)
		if pr.pr15().bit_is_set() {
			let data_bit = ctx.shared.ps2_dat0.is_high().unwrap();
			// Do we have a complete word?
			if let Some(data) = ctx.shared.kb_decoder.lock(|r| r.add_bit(data_bit)) {
				// Don't dump in the ISR - we're busy. Add it to this nice lockless queue instead.
				if ctx
					.shared
					.msg_q_in
					.lock(|q| q.enqueue(Message::Ps2Data0(data)))
					.is_err()
				{
					panic!("queue full");
				};
			}
			// Clear the pending flag for this pin
			ctx.shared.exti.pr.write(|w| w.pr15().set_bit());
		}

		if pr.pr4().bit_is_set() {
			let msg = if ctx.shared.pin_cs.lock(|pin| pin.is_low().unwrap()) {
				// If incoming Chip Select is low, tell the main thread to turn on the SPI engine
				Message::SpiEnable
			} else {
				// If incoming Chip Select is high, tell the main thread to turn off the SPI engine
				Message::SpiDisable
			};
			if ctx.shared.msg_q_in.lock(|q| q.enqueue(msg)).is_err() {
				panic!("queue full");
			}
			// Clear the pending flag for this pin
			ctx.shared.exti.pr.write(|w| w.pr4().set_bit());
		}
	}

	/// This is the USART1 task.
	///
	/// It fires whenever there is new data received on USART1. We should flag to the host
	/// that data is available.
	#[task(binds = USART1, shared = [serial, msg_q_in])]
	fn usart1_interrupt(mut ctx: usart1_interrupt::Context) {
		// Reading the register clears the RX-Not-Empty-Interrupt flag.
		if let Ok(b) = ctx.shared.serial.read() {
			let _ = ctx
				.shared
				.msg_q_in
				.lock(|q| q.enqueue(Message::UartByte(b)));
		}
	}

	/// Initialization melody, played directly by the BMC
	#[task(shared = [speaker, msg_q_in])]
	fn speaker_init_tune(mut ctx: speaker_init_tune::Context) {
		defmt::trace!("Playing startup tone");

		ctx.shared.speaker.lock(|speaker| {
			// F4
			speaker.set_note(100, 137, 10);
		});

		speaker_pwm_stop::spawn_after(100.millis()).unwrap();
	}
	/// Task which stops the speaker from playing
	#[task(shared = [msg_q_in])]
	fn speaker_pwm_stop(mut ctx: speaker_pwm_stop::Context) {
		defmt::trace!("Speaker stopped");
		let _ = ctx
			.shared
			.msg_q_in
			.lock(|q| q.enqueue(Message::SpeakerDisable));
	}

	/// This is the SPI1 task.
	///
	/// It fires whenever there is new data received on SPI1. We should flag to the host
	/// that data is available.
	#[task(binds = SPI1, shared = [spi, msg_q_in])]
	fn spi1_interrupt(mut ctx: spi1_interrupt::Context) {
		let has_message = ctx.shared.spi.lock(|spi| spi.handle_isr());
		if has_message {
			let _ = ctx.shared.msg_q_in.lock(|q| q.enqueue(Message::SpiRx));
		}
	}

	/// This is the LED blink task.
	///
	/// This task is called periodically. We check whether the status LED is currently on or off,
	/// and set it to the opposite. This makes the LED blink.
	#[task(shared = [led_power, state_dc_power_enabled], local = [ led_state: bool = false ])]
	fn led_power_blink(mut ctx: led_power_blink::Context) {
		let dc_power_state = ctx.shared.state_dc_power_enabled.lock(|r| *r);
		match dc_power_state {
			DcPowerState::Off => {
				if *ctx.local.led_state {
					ctx.shared.led_power.set_low().unwrap();
					*ctx.local.led_state = false;
				} else {
					ctx.shared.led_power.set_high().unwrap();
					*ctx.local.led_state = true;
				}
				led_power_blink::spawn_after(LED_PERIOD_MS.millis()).unwrap();
			}
			DcPowerState::On | DcPowerState::Starting => {
				ctx.shared.led_power.set_high().unwrap();
			}
		}
	}

	/// This task polls our power and reset buttons.
	///
	/// We poll them rather than setting up an interrupt as we need to debounce
	/// them, which involves waiting a short period and checking them again.
	/// Given that we have to do that, we might as well not bother with the
	/// interrupt.
	#[task(
		shared = [
			led_power, button_power, button_reset, msg_q_in, kb_decoder
		],
		local = [ press_button_power_short, press_button_power_long, press_button_reset_short ]
	)]
	fn button_poll(mut ctx: button_poll::Context) {
		// Poll buttons
		let pwr_pressed: bool = ctx.shared.button_power.is_low().unwrap();
		let rst_pressed: bool = ctx.shared.button_reset.is_low().unwrap();

		// Poll PS2
		ctx.shared.kb_decoder.lock(|r| r.poll());

		// Update state
		let pwr_short_edge = ctx.local.press_button_power_short.update(pwr_pressed);
		let pwr_long_edge = ctx.local.press_button_power_long.update(pwr_pressed);
		let rst_long_edge = ctx.local.press_button_reset_short.update(rst_pressed);

		defmt::trace!(
			"pwr/rst {}/{} {}/{}/{}",
			pwr_pressed,
			rst_pressed,
			match pwr_short_edge {
				Some(debouncr::Edge::Rising) => "r",
				Some(debouncr::Edge::Falling) => "f",
				None => "-",
			},
			match pwr_long_edge {
				Some(debouncr::Edge::Rising) => "r",
				Some(debouncr::Edge::Falling) => "f",
				None => "-",
			},
			match rst_long_edge {
				Some(debouncr::Edge::Rising) => "r",
				Some(debouncr::Edge::Falling) => "f",
				None => "-",
			}
		);

		if pwr_long_edge == Some(debouncr::Edge::Rising) {
			// They pressed it a really long time
			let _ = ctx
				.shared
				.msg_q_in
				.lock(|q| q.enqueue(Message::PowerButtonLongPress));
		}

		match pwr_short_edge {
			Some(debouncr::Edge::Rising) => {
				// They pressed the power button (could be a short press, could be a long press)
				let _ = ctx
					.shared
					.msg_q_in
					.lock(|q| q.enqueue(Message::PowerButtonShortPress));
			}
			Some(debouncr::Edge::Falling) => {
				// They released the power button
				let _ = ctx
					.shared
					.msg_q_in
					.lock(|q| q.enqueue(Message::PowerButtonRelease));
			}
			_ => {
				// Ignore
			}
		}

		if rst_long_edge == Some(debouncr::Edge::Rising) {
			// They pressed the reset button.
			let _ = ctx
				.shared
				.msg_q_in
				.lock(|q| q.enqueue(Message::ResetButtonShortPress));
		}

		// Re-schedule the timer interrupt
		button_poll::spawn_after(DEBOUNCE_POLL_INTERVAL_MS.millis()).unwrap();
	}

	/// Return the reset line high (inactive), but only if we're still powered on.
	#[task(shared = [pin_sys_reset, state_dc_power_enabled])]
	fn exit_reset(mut ctx: exit_reset::Context) {
		defmt::debug!("End reset");
		if ctx.shared.state_dc_power_enabled.lock(|r| *r) != DcPowerState::Off {
			// Raising the reset line takes the rest of the system out of reset
			ctx.shared.pin_sys_reset.lock(|pin| pin.set_high().unwrap());
		}
	}
}

/// Process an incoming command, converting a request into a response.
fn process_command<F>(req: proto::Request, register_state: &mut RegisterState, rsp_handler: F)
where
	F: FnOnce(&proto::Response),
{
	if register_state.last_req.as_ref() == Some(&req) {
		// A duplicate! Resend what we sent last time (so we don't affect FIFOs with a duplicate read).
		let length = req.length_or_data as usize;
		let rsp = proto::Response::new_ok_with_data(&register_state.scratch[0..length]);
		defmt::debug!("Detected a retry");
		rsp_handler(&rsp);
		return;
	}

	// We were not sent what we were sent last time, so forget the previous request.
	register_state.last_req = None;

	// temporary buffer to hold serialized data while the response is generated
	let mut data = [0u8; 1];

	// What do they want?
	let rsp = match (req.request_type.flatten(), Command::try_from(req.register)) {
		(proto::RequestType::Read, Ok(Command::ProtocolVersion)) => {
			defmt::trace!("Reading ProtocolVersion");
			// They want the Protocol Version we support. Give them v0.1.1.
			let length = req.length_or_data as usize;
			if length == 3 {
				// No need to cache
				proto::Response::new_ok_with_data(&[0, 1, 1])
			} else {
				proto::Response::new_without_data(proto::ResponseResult::BadLength)
			}
		}
		(proto::RequestType::Read, Ok(Command::FirmwareVersion)) => {
			defmt::trace!("Reading FirmwareVersion");
			// They want the Firmware Version string.
			let length = req.length_or_data as usize;
			if length <= register_state.firmware_version.len() {
				let bytes = &register_state.firmware_version;
				// No need to cache
				proto::Response::new_ok_with_data(&bytes[0..length])
			} else {
				proto::Response::new_without_data(proto::ResponseResult::BadLength)
			}
		}
		(proto::RequestType::Read, Ok(Command::Ps2KbBuffer)) => {
			defmt::trace!("Reading Ps2KbBuffer");
			let length = req.length_or_data as usize;
			if length > 0 && length <= register_state.scratch.len() {
				// First byte is the # bytes in the FIFO
				register_state.scratch[0] = register_state.ps2_kb_bytes.len() as u8;
				// Then as many of those FIFO bytes as fit
				for slot in &mut register_state.scratch[1..] {
					if let Some(x) = register_state.ps2_kb_bytes.pop_front() {
						*slot = x;
					} else {
						*slot = 0;
					}
				}
				// OK, cache this one because FIFO reads are damaing.
				register_state.last_req = Some(req);
				// Send the response
				proto::Response::new_ok_with_data(&register_state.scratch[0..length])
			} else {
				// Can't help you - you want a weird number of bytes
				proto::Response::new_without_data(proto::ResponseResult::BadLength)
			}
		}
		(proto::RequestType::Read, Ok(Command::SpeakerDuration)) => {
			defmt::debug!("Reading speaker duration");
			data[0] = (register_state.speaker.duration() / 10) as u8;
			proto::Response::new_ok_with_data(&data)
		}
		(proto::RequestType::ShortWrite, Ok(Command::SpeakerDuration)) => {
			defmt::debug!("Writing speaker duration ({})", req.length_or_data);
			// This update actually causes the speaker to beep
			register_state
				.speaker
				.set_duration(req.length_or_data as u16 * 10);
			proto::Response::new_without_data(proto::ResponseResult::Ok)
		}
		(proto::RequestType::Read, Ok(Command::SpeakerPeriodHigh)) => {
			defmt::debug!("Reading speaker period (high)");
			data[0] = register_state.speaker.period_high();
			proto::Response::new_ok_with_data(&data)
		}
		(proto::RequestType::ShortWrite, Ok(Command::SpeakerPeriodHigh)) => {
			defmt::debug!("Writing speaker period (high = {})", req.length_or_data);
			register_state.speaker.set_period_low(req.length_or_data);
			proto::Response::new_without_data(proto::ResponseResult::Ok)
		}
		(proto::RequestType::Read, Ok(Command::SpeakerPeriodLow)) => {
			defmt::debug!("Reading speaker period (low)");
			data[0] = register_state.speaker.period_low();
			proto::Response::new_ok_with_data(&data)
		}
		(proto::RequestType::ShortWrite, Ok(Command::SpeakerPeriodLow)) => {
			defmt::debug!("Writing speaker period (low = {})", req.length_or_data);
			register_state.speaker.set_period_high(req.length_or_data);
			proto::Response::new_without_data(proto::ResponseResult::Ok)
		}
		(proto::RequestType::Read, Ok(Command::SpeakerDutyCycle)) => {
			defmt::debug!("Reading speaker duty cycle");
			data[0] = register_state.speaker.duty_cycle();
			proto::Response::new_ok_with_data(&data)
		}
		(proto::RequestType::ShortWrite, Ok(Command::SpeakerDutyCycle)) => {
			defmt::debug!("Writing speaker duty cycle ({})", req.length_or_data);
			register_state.speaker.set_duty_cycle(req.length_or_data);
			proto::Response::new_without_data(proto::ResponseResult::Ok)
		}
		_ => {
			// Sorry, that register / request type is not supported
			defmt::warn!(
				"Unknown register operation {:?} on 0x{:02x}",
				req.request_type,
				req.register
			);
			proto::Response::new_without_data(proto::ResponseResult::BadRegister)
		}
	};
	rsp_handler(&rsp);
	// defmt::debug!("Sent {:?}", rsp);
}

// End of file
