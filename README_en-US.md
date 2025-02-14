# SmartDNS-rs

English | [中文](https://github.com/mokeyish/smartdns-rs/blob/master/README_zh-CN.md)

A local DNS server imspired by [c smartdns](https://github.com/pymumu/smartdns) to accepts DNS query requests from local clients, obtains DNS query results from multiple upstream DNS servers, and returns the fastest access results to clients.
Avoiding DNS pollution and improving network access speed, supports high-performance ad filtering.

Note: The c version of smartdns is very functional, but because it only supports **Linux**, while **MacOS and Windows** can only be supported through Docker or WSL. Therefore, I want to develop a rust version of SmartDNS that supports compiling to Windows, MacOS, Linux and Android Termux environment to run, and is compatible with its configuration.

---

**Currently under development, please do not use in production environment.**

## Configuration parameter

Please refer to [here](https://github.com/pymumu/smartdns/blob/master/ReadMe_en.md#configuration-parameter) for configuration.

## Building

Open your terminal and execute these commands:

```shell
git clone https://github.com/mokeyish/smartdns-rs.git
cd smartdns-rs
cargo build --release
```

## Others

TODO...

## Acknowledgments !!!  

This software wouldn't have been possible without:

- [Trust-DNS](https://github.com/bluejekyll/trust-dns)
- [SmartDNS](https://github.com/pymumu/smartdns)



## License

This software contains codes from https://github.com/bluejekyll/trust-dns, which is licensed under either of


- Apache License, Version 2.0, (LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license (LICENSE-MIT or http://opensource.org/licenses/MIT)


And other codes is licensed under

- GPL-3.0 license (LICENSE-GPL-3.0 or https://opensource.org/licenses/GPL-3.0)
