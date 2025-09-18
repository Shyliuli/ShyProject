#pragma once
#include "common.hpp"
class first_scanner {
private:
    std::string codes;
    first_scanner(std::string codes){
        this->codes = codes;
    }
public:
    static fn create(std::string codes)->Result<unique_ptr<first_scanner>,CoreError>
    {
        auto fs=unique_ptr<first_scanner>
        (new(std::nothrow) first_scanner(codes));
        if(fs==nullptr){
            return Err<unique_ptr<first_scanner>>(
                CoreError(
                    AllocError("first_scanner alloc error")
                )
            );
        }
        return Ok<CoreError>(std::move(fs));
    }
    
};


