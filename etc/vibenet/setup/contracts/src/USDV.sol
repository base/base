// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title USDV - Vibe USD
/// @notice Public-mint ERC-20 used on vibenet as a stand-in for USDC. Anyone
///         can mint up to `MAX_MINT_PER_CALL` to any address; the faucet
///         service uses this to "drip" test dollars, but any caller may do
///         the same directly. Not audited - do not deploy to production.
/// @dev    6 decimals to match USDC so UIs and integrations can treat it as
///         a drop-in replacement. The symbol is USDV to make it obvious the
///         balance is worthless.
contract USDV {
    string public constant name = "Vibe USD";
    string public constant symbol = "USDV";
    uint8 public constant decimals = 6;
    uint256 public constant MAX_MINT_PER_CALL = 1_000_000 * 10 ** 6;

    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    function mint(address to, uint256 amount) external {
        require(amount <= MAX_MINT_PER_CALL, "mint too large");
        balanceOf[to] += amount;
        totalSupply += amount;
        emit Transfer(address(0), to, amount);
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        require(allowed >= amount, "allowance");
        if (allowed != type(uint256).max) {
            allowance[from][msg.sender] = allowed - amount;
        }
        _transfer(from, to, amount);
        return true;
    }

    function _transfer(address from, address to, uint256 amount) internal {
        require(balanceOf[from] >= amount, "balance");
        unchecked {
            balanceOf[from] -= amount;
            balanceOf[to] += amount;
        }
        emit Transfer(from, to, amount);
    }
}
