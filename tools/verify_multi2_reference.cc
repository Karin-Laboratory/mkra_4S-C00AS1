// Reproduce synthetic vectors with the pinned upstream implementation.
#include <cstddef>
#include <cstdio>
#include "multi2_compat.h"
#include "multi2_cipher.h"
int main() {
    multi2::system_key_type s;
    for(unsigned i=0;i<8;++i) s[i]=(i*4u<<24)|((i*4u+1)<<16)|((i*4u+2)<<8)|(i*4u+3);
    multi2::data_key_type d;d[0]=0x80818283;d[1]=0x84858687;
    multi2::iv_type iv;iv[0]=0xf0f1f2f3;iv[1]=0xf4f5f6f7;
    auto key=multi2::schedule(d,s);
    for(unsigned n: {1,7,8,9,15,16,17,183,184}) {
        unsigned char b[184];for(unsigned i=0;i<n;++i)b[i]=(i*37+11)&255;
        multi2::encrypt_cbc_ofb(b,n,iv,key,4);
        std::printf("%u ",n);for(unsigned i=0;i<n;++i)std::printf("%02x",b[i]);std::puts("");
    }
}
