/// Fuzz testing logic defined in this module
/// This module depends on the default configuration file `flowgger.toml`
/// 
/// The test in this module hits flowgger with random input. 
/// The expected state is a failure for invalid inputs (no logs sent to output) 
/// and successfully parsed logs written to output stream for valid inputs
/// 
/// # Dependencies
/// It depend on the external crates [`QuickCheck`][https://docs.rs/quickcheck/latest/quickcheck/] and 
/// [`LazyStatic`][https://docs.rs/lazy_static/latest/lazy_static/]
/// 
/// `QuickCheck`is used to generate random string input, while LazyStatic is used to lazily intialize share resources at runtime
/// 
/// # Errors
///
/// This function will return an error if the default config does not exists,is unreadbale, or is not valid
/// toml format
#[cfg(test)]
mod tests {
    extern crate quickcheck;

    use crate::flowgger;

    use quickcheck::QuickCheck;

    use std::sync::mpsc::{Receiver, sync_channel, SyncSender};
    use std::sync::{Arc, Mutex};
    use std::{fs};
    use std::io::{BufReader, BufRead};
    use std::sync::Once;

    use flowgger::config::Config;
    use flowgger::encoder::Encoder;
    use flowgger::decoder::Decoder;
    use flowgger::merger;
    use flowgger::output;
    use flowgger::get_output_file;
    use flowgger::get_decoder_rfc3164;
    use flowgger::get_encoder_rfc3164;
    use flowgger::input::udp_input::handle_record_maybe_compressed;

    use self::merger::{LineMerger, Merger};
    use toml::Value;

    const DEFAULT_CONFIG_FILE: &str = "flowgger.toml";
    const DEFAULT_OUTPUT_FILEPATH: &str = "output.log";
    const DEFAULT_QUEUE_SIZE: usize = 10_000_000;

    const DEFAULT_OUTPUT_FORMAT: &str = "gelf";
    const DEFAULT_OUTPUT_FRAMING: &str = "noop";
    const DEFAULT_OUTPUT_TYPE: &str = "file";

    const DEFAULT_FUZZED_MESSAGE_COUNT: u64 = 500;


    static INIT_CONFIG: Once = Once::new();
    static mut GLOB_CONFIG: Option<Config> = None;

    static INIT_FILEPATH: Once = Once::new();
    static mut GLOB_FILEPATH: String = String::new();

    static INIT_DECODER: Once = Once::new();
    static mut GLOB_DECODER: Mutex<Option<Box<dyn Decoder>>> = Mutex::new(None);

    static INIT_ENCODER: Once = Once::new();
    static mut GLOB_ENCODER: Mutex<Option<Box<dyn Encoder>>> = Mutex::new(None);

    static INIT_SYNC_SENDER: Once = Once::new();
    static mut GLOB_SYNC_SENDER: Mutex<Option<SyncSender<Vec<u8>>>> = Mutex::new(None);

    #[test]
    fn test_fuzzer(){
        let config = get_config();
        let file_output_path = config.lookup("output.file_path").map_or(DEFAULT_OUTPUT_FILEPATH, |x| {
            x.as_str().expect("File output path missing in config")
        });
        remove_output_file(&file_output_path);

        let (tx, rx): (SyncSender<Vec<u8>>, Receiver<Vec<u8>>) = sync_channel(DEFAULT_QUEUE_SIZE);
        start_file_output(&config, rx);
        set_globals(&config, &file_output_path, tx);

        QuickCheck::new().tests(DEFAULT_FUZZED_MESSAGE_COUNT).quickcheck(fuzz_target_rfc3164 as fn(String));
        let _ = check_result(&file_output_path);
    }

    fn set_globals(config: &Config, file_output_path: &str, sync_sender: SyncSender<Vec<u8>>){
        set_global_config(config.clone());
        set_global_output_filepath(file_output_path.to_string());
        set_global_rfc3164_decoder(config);
        set_global_rfc3164_encoder(config);
        set_global_sync_sender(sync_sender);
    }

    fn set_global_rfc3164_decoder(config: &Config) {
        INIT_DECODER.call_once(|| {
            unsafe{
                let decoder = get_decoder_rfc3164(config);
                let mut guard = GLOB_DECODER.lock().unwrap();
                if guard.is_none(){
                    *guard = Some(decoder.clone_boxed());
                }
                drop(guard);
            }
        });
    }

    fn set_global_rfc3164_encoder(config: &Config) {
        INIT_ENCODER.call_once(|| {
            unsafe{
                let encoder = get_encoder_rfc3164(config);
                let mut guard = GLOB_ENCODER.lock().unwrap();
                if guard.is_none(){
                    *guard = Some(encoder.clone_boxed());
                }
                drop(guard);
            }
        });
    }

    fn set_global_sync_sender(tx: SyncSender<Vec<u8>>){
        INIT_SYNC_SENDER.call_once( || {
            unsafe{
                let mut guard = GLOB_SYNC_SENDER.lock().unwrap();
                if guard.is_none(){
                    *guard = Some(tx);
                }
                drop(guard);
            }
        });
    }

    fn set_global_output_filepath(filepath: String) {
        INIT_FILEPATH.call_once(|| {
            unsafe{
                GLOB_FILEPATH = filepath;
            }
        });
    }

    fn set_global_config(config: Config){
        INIT_CONFIG.call_once(|| {
            unsafe{
                GLOB_CONFIG = Some(config);
            }
        });
    }

    fn get_config() -> Config {
        let mut config = match Config::from_path(DEFAULT_CONFIG_FILE) {
            Ok(config) => config,
            Err(e) => panic!(
                "Unable to read the config file [{}]: {}",
                "flowgger.toml",
                e.to_string()
            ),
        };

        update_file_rotation_defaults_in_config(&mut config);
        return config;
    }

    /// Update the default file rotation size and time in the config file
    /// This ensures output is sent to a single non-rotated file
    pub fn update_file_rotation_defaults_in_config(config: &mut Config){
        if let Some(entry) = config.config.get_mut("output").unwrap().get_mut("file_rotation_size"){
            *entry = Value::Integer(0);
        }

        if let Some(entry) = config.config.get_mut("output").unwrap().get_mut("file_rotation_time"){
            *entry = Value::Integer(0);
        }
    }

    pub fn remove_output_file(file_output_path: &str){
        let _ = fs::remove_file(file_output_path);
    }

    /// Start an input listener which writes data to the output file once received.
    pub fn start_file_output(config: &Config, rx: Receiver<Vec<u8>>){

        let output_format = config
            .lookup("output.format")
            .map_or(DEFAULT_OUTPUT_FORMAT, |x| {
                x.as_str().expect("output.format must be a string")
            });

        let output = get_output_file(&config);
        let output_type = config
            .lookup("output.type")
            .map_or(DEFAULT_OUTPUT_TYPE, |x| {
                x.as_str().expect("output.type must be a string")
            });

        let _output_framing = match config.lookup("output.framing") {
            Some(framing) => framing.as_str().expect("output.framing must be a string"),
            None => match (output_format, output_type) {
                ("capnp", _) | (_, "kafka") => "noop",
                (_, "debug") | ("ltsv", _) => "line",
                ("gelf", _) => "nul",
                _ => DEFAULT_OUTPUT_FRAMING,
            },
        };
        let merger: Option<Box<dyn Merger>> = Some(Box::new(LineMerger::new(&config)) as Box<dyn Merger>);

        let arx = Arc::new(Mutex::new(rx));
        output.start(arx, merger);
    }

    pub fn fuzz_target_rfc3164(data: String) {
        unsafe{
            let mut sender_guard = match GLOB_SYNC_SENDER.lock() {
                Ok(guard) => guard,
                Err(_poisoned_error) => {
                    // Handle the poisoned Mutex
                    let guard = _poisoned_error.into_inner();
                    guard
                }
            };
            let sync_sender: &mut SyncSender<Vec<u8>> = sender_guard.as_mut().unwrap();

            let mut encoder_guard = match GLOB_ENCODER.lock() {
                Ok(guard) => guard,
                Err(_poisoned_error) => {
                    let guard = _poisoned_error.into_inner();
                    guard
                }
            };
            let encoder: &mut Box<dyn Encoder> = encoder_guard.as_mut().unwrap();

            let mut decoder_guard = match GLOB_DECODER.lock() {
                Ok(guard) => guard,
                Err(_poisoned_error) => {
                    let guard = _poisoned_error.into_inner();
                    guard
                }
            };
            let decoder: &mut Box<dyn Decoder> = decoder_guard.as_mut().unwrap();

            let _result = handle_record_maybe_compressed(data.as_bytes(), &sync_sender, &decoder, &encoder);
        }
    }

    // Check for the result
    // On invalid input, no logs are expected to be written to the output file
    // For valid inputs, analyze each log entry, and check that the hostnames and appnames are in place
    fn check_result(file_output_path: &str)-> Result<(), std::io::Error> {
        unsafe{

            let mut sender_guard = match GLOB_SYNC_SENDER.lock() {
                Ok(guard) => guard,
                Err(_poisoned_error) => {
                    let guard = _poisoned_error.into_inner();
                    guard
                }
            };
            let tx: SyncSender<Vec<u8>> = sender_guard.take().unwrap();
            drop(tx);

            let file = fs::File::open(file_output_path).expect("Unable to open output file");
            let reader = BufReader::new(file);

            for line in reader.lines() {
                let line_item: String = line?;
                if !line_item.trim().is_empty() {
                    let split_line_content: Vec<&str> = line_item.split(" ").filter(|data| !data.is_empty()).collect();
                    let hostname = split_line_content[3].trim();
                    let appname = split_line_content[4].trim();

                    if hostname.is_empty() || appname.is_empty() {
                        panic!("Log output invalid");
                    }
                }
                
            }
            Ok(())
        }
    }
}




