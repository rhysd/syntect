use bincode::SizeLimit;
use bincode::rustc_serialize::*;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use parsing::PackageSet;
use highlighting::ThemeSet;
use std::path::Path;
use flate2::write::ZlibEncoder;
use flate2::read::ZlibDecoder;
use flate2::Compression;
use rustc_serialize::{Encodable, Decodable};

pub fn dump_binary<T: Encodable>(o: &T) -> Vec<u8> {
    let mut v = Vec::new();
    {
        let mut encoder = ZlibEncoder::new(&mut v, Compression::Best);
        encode_into(o, &mut encoder, SizeLimit::Infinite).unwrap();
    }
    v
}

pub fn dump_to_file<T: Encodable, P: AsRef<Path>>(o: &T, path: P) -> EncodingResult<()> {
    let f = BufWriter::new(try!(File::create(path).map_err(EncodingError::IoError)));
    let mut encoder = ZlibEncoder::new(f, Compression::Best);
    encode_into(o, &mut encoder, SizeLimit::Infinite)
}

/// Returns a fully loaded and linked package set from
/// a binary dump. Panics if the dump is invalid.
pub fn from_binary<T: Decodable>(v: &[u8]) -> T {
    let mut decoder = ZlibDecoder::new(v);
    decode_from(&mut decoder, SizeLimit::Infinite).unwrap()
}

/// Returns a fully loaded and linked package set from
/// a binary dump file.
pub fn from_dump_file<T: Decodable, P: AsRef<Path>>(path: P) -> DecodingResult<T> {
    let f = try!(File::open(path).map_err(DecodingError::IoError));
    let mut decoder = ZlibDecoder::new(BufReader::new(f));
    decode_from(&mut decoder, SizeLimit::Infinite)
}

impl PackageSet {
    /// Instantiates a new package set from a binary dump of
    /// Sublime Text's default open source syntax definitions and then links it.
    /// These dumps are included in this library's binary for convenience.
    /// This method loads the version for parsing line strings with no `\n` characters at the end.
    ///
    /// This is the recommended way of creating a package set for
    /// non-advanced use cases. It is also significantly faster than loading the YAML files.
    ///
    /// Note that you can load additional syntaxes after doing this,
    /// you'll just have to link again. If you want you can even
    /// use the fact that SyntaxDefinitions are serializable with
    /// the bincode crate to cache dumps of additional syntaxes yourself.
    pub fn load_defaults_nonewlines() -> PackageSet {
        let mut ps: PackageSet = from_binary(include_bytes!("../assets/default_nonewlines.\
                                                             packdump"));
        ps.link_syntaxes();
        ps
    }

    /// Same as `load_defaults_nonewlines` but for parsing line strings with newlines at the end.
    /// These are separate methods because thanks to linker garbage collection, only the serialized
    /// dumps for the method(s) you call will be included in the binary (each is ~200kb for now).
    pub fn load_defaults_newlines() -> PackageSet {
        let mut ps: PackageSet = from_binary(include_bytes!("../assets/default_newlines.packdump"));
        ps.link_syntaxes();
        ps
    }
}

impl ThemeSet {
    /// Loads the set of default themes
    /// Currently includes Solarized light/dark, Base16 ocean/mocha/eighties and InspiredGithub
    pub fn load_defaults() -> ThemeSet {
        from_binary(include_bytes!("../assets/default.themedump"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parsing::PackageSet;
    use highlighting::ThemeSet;
    #[test]
    fn can_dump_and_load() {
        let mut ps = PackageSet::new();
        ps.load_syntaxes("testdata/Packages", false).unwrap();

        let bin = dump_binary(&ps);
        let ps2: PackageSet = from_binary(&bin[..]);
        assert_eq!(ps.syntaxes.len(), ps2.syntaxes.len());

        let themes = ThemeSet::load_defaults();
        assert!(themes.themes.len() > 4);
    }
}
