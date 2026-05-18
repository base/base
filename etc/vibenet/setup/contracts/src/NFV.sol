// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title NFV - Non-Fungible Vibe
/// @notice Minimal public-mint ERC-721 used on vibenet for demos. Anyone can
///         mint the next token to any address. Not audited; no metadata.
contract NFV {
    string public constant name = "Non-Fungible Vibe";
    string public constant symbol = "NFV";

    uint256 public nextTokenId;

    mapping(uint256 => address) private _owners;
    mapping(address => uint256) private _balances;
    mapping(uint256 => address) private _tokenApprovals;
    mapping(address => mapping(address => bool)) private _operatorApprovals;

    event Transfer(address indexed from, address indexed to, uint256 indexed tokenId);
    event Approval(address indexed owner, address indexed approved, uint256 indexed tokenId);
    event ApprovalForAll(address indexed owner, address indexed operator, bool approved);

    function mint(address to) external returns (uint256 tokenId) {
        tokenId = nextTokenId++;
        _owners[tokenId] = to;
        _balances[to] += 1;
        emit Transfer(address(0), to, tokenId);
    }

    function ownerOf(uint256 tokenId) external view returns (address) {
        address owner = _owners[tokenId];
        require(owner != address(0), "nonexistent");
        return owner;
    }

    function balanceOf(address owner) external view returns (uint256) {
        require(owner != address(0), "zero");
        return _balances[owner];
    }

    function approve(address to, uint256 tokenId) external {
        address owner = _owners[tokenId];
        require(
            owner == msg.sender || _operatorApprovals[owner][msg.sender],
            "not authorized"
        );
        _tokenApprovals[tokenId] = to;
        emit Approval(owner, to, tokenId);
    }

    function getApproved(uint256 tokenId) external view returns (address) {
        require(_owners[tokenId] != address(0), "nonexistent");
        return _tokenApprovals[tokenId];
    }

    function setApprovalForAll(address operator, bool approved) external {
        _operatorApprovals[msg.sender][operator] = approved;
        emit ApprovalForAll(msg.sender, operator, approved);
    }

    function isApprovedForAll(address owner, address operator) external view returns (bool) {
        return _operatorApprovals[owner][operator];
    }

    function transferFrom(address from, address to, uint256 tokenId) external {
        require(_owners[tokenId] == from, "wrong from");
        require(
            msg.sender == from
                || _tokenApprovals[tokenId] == msg.sender
                || _operatorApprovals[from][msg.sender],
            "not authorized"
        );
        _tokenApprovals[tokenId] = address(0);
        _owners[tokenId] = to;
        unchecked {
            _balances[from] -= 1;
            _balances[to] += 1;
        }
        emit Transfer(from, to, tokenId);
    }

    function supportsInterface(bytes4 iid) external pure returns (bool) {
        return iid == 0x80ac58cd // ERC-721
            || iid == 0x01ffc9a7; // ERC-165
    }
}
