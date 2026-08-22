// SPDX-License-Identifier: MIT
pragma solidity 0.8.25;

/**
 * @title OptimizedCrossDomainSender
 * @notice Base ekosistemi için optimize edilmiş güvenli L1-L2 mesajlaşma örneği.
 * @author Mehmet Çelik (temhemc) | BaseKit
 */
contract OptimizedCrossDomainSender {

    // Gas tasarrufu sağlayan Custom Error yapıları
    error ZeroAddressNotAllowed();
    error TransferFailed();
    error UnauthorizedCaller();

    address public immutable MESSENGER;
    address public owner;

    event MessageDispatched(address indexed target, uint256 value, bytes data);

    modifier onlyOwner() {
        if (msg.sender != owner) revert UnauthorizedCaller();
        _;
    }

    constructor(address _messenger) {
        if (_messenger == address(0)) revert ZeroAddressNotAllowed();
        MESSENGER = _messenger;
        owner = msg.sender;
    }

    /**
     * @notice L2 üzerinden L1'e güvenli veri ve işlem aktarımı tetikler.
     */
    function dispatchCrossDomainMessage(
        address _target,
        uint256 _value,
        bytes calldata _data
    ) external payable onlyOwner {
        if (_target == address(0)) revert ZeroAddressNotAllowed();

        // Örnek köprü entegrasyon mantığı ve event tetikleme
        emit MessageDispatched(_target, _value, _data);
    }
}
